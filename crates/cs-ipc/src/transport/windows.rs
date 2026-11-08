//! Transporte en Windows: named pipe local restringida al usuario actual.
//!
//! Medidas de seguridad (lección de la tubería del VDD, ver
//! `docs/poc-results/poc-01-vdd.md`, que concedía control total a Everyone):
//!
//! - **DACL** `D:P(A;;GA;;;<SID del usuario>)(A;;GA;;;SY)`: solo el usuario que
//!   lanza el agente y SYSTEM pueden abrir la tubería. `P` (protegida) impide
//!   heredar ACEs. Sin etiqueta de integridad explícita, el objeto queda en
//!   integridad media: los procesos de baja integridad del mismo usuario
//!   (sandbox de navegadores) no pueden escribir en ella.
//! - **Propietario** `O:<SID del usuario>` explícito, para que el cliente
//!   pueda comprobarlo.
//! - **`PIPE_REJECT_REMOTE_CLIENTS`**: no se aceptan conexiones por SMB.
//! - **`FILE_FLAG_FIRST_PIPE_INSTANCE`** en la primera instancia: si el nombre
//!   ya existe (otro agente u otro proceso que intenta suplantarlo) el agente
//!   no arranca.
//! - El **cliente** comprueba que el propietario de la tubería a la que se
//!   conecta es el usuario actual (o el grupo Administradores, que ya controla
//!   la máquina) antes de enviar nada: así un proceso de otro usuario que
//!   cree antes el nombre no recibe las peticiones (p. ej. un PIN).
//!   Además tokio abre el cliente con `SECURITY_IDENTIFICATION`, de modo que
//!   el servidor no puede suplantar al cliente.
//! - El nombre incluye el SID del usuario, de modo que cada usuario de la
//!   máquina tiene su propio agente.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::time::Duration;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_PIPE_BUSY, HANDLE, HLOCAL, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// SID del grupo integrado Administradores.
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const SDDL_REVISION_1: u32 = 1;

pub type ServerStream = NamedPipeServer;
pub type ClientStream = NamedPipeClient;

/// SID (forma `S-1-5-21-…`) del usuario del proceso actual.
pub fn current_user_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess devuelve un pseudo-handle válido; `token` es
    // un puntero válido donde la API escribe el handle abierto.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(win_err)?;
    let token = OwnedHandle(token);

    let mut len = 0u32;
    // SAFETY: consulta del tamaño necesario: buffer nulo y longitud 0; la API
    // devuelve error INSUFFICIENT_BUFFER (esperado) y rellena `len`.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // Buffer alineado a u64 para poder interpretarlo como TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` tiene al menos `len` bytes y vive durante la llamada.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            len,
            &mut len,
        )
    }
    .map_err(win_err)?;
    // SAFETY: la API ha escrito una estructura TOKEN_USER al inicio de `buf`
    // (alineado a 8, suficiente para TOKEN_USER); el SID apunta dentro de `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_to_string(sid)
}

fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut s = PWSTR::null();
    // SAFETY: `sid` es un SID válido (lo garantiza el llamante); la API
    // reserva la cadena con LocalAlloc y la liberamos abajo con LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &mut s) }.map_err(win_err)?;
    // SAFETY: `s` es una cadena UTF-16 terminada en nulo devuelta por la API.
    let out = unsafe { s.to_string() }
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SID no UTF-16"));
    // SAFETY: memoria reservada por ConvertSidToStringSidW con LocalAlloc.
    unsafe { LocalFree(HLOCAL(s.0.cast())) };
    out
}

/// Nombre de la tubería del agente para el usuario actual.
pub fn default_pipe_name() -> io::Result<String> {
    Ok(format!(
        r"\\.\pipe\crossscreen-agent-{}",
        current_user_sid()?
    ))
}

/// Descriptor de seguridad autocontenido creado desde SDDL.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn for_current_user() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl = format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` es una cadena UTF-16 terminada en nulo que vive
        // durante la llamada; la API reserva `sd` con LocalAlloc (se libera en Drop).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(win_err)?;
        Ok(SecurityDescriptor(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: reservado por ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(HLOCAL(self.0 .0)) };
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: handle abierto por nosotros y no compartido.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn win_err(e: windows::core::Error) -> io::Error {
    // HRESULT_FROM_WIN32: los 16 bits bajos son el código Win32.
    let code = e.code().0;
    if (code as u32) & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error(code & 0xFFFF)
    } else {
        io::Error::other(e.to_string())
    }
}

/// Crea una instancia de servidor de la tubería con la DACL restringida.
/// `first = true` exige que sea la primera instancia (instancia única).
pub fn create_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let sd = SecurityDescriptor::for_current_user()?;
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 .0,
        bInheritHandle: false.into(),
    };
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: `attrs` es un SECURITY_ATTRIBUTES válido cuyo descriptor (`sd`)
    // sigue vivo durante la llamada; CreateNamedPipeW copia lo que necesita.
    unsafe {
        opts.create_with_security_attributes_raw(
            name,
            (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

/// Indica si el error al crear la primera instancia significa que el nombre
/// ya está en uso (otro agente en ejecución).
pub fn is_name_in_use(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5) con FILE_FLAG_FIRST_PIPE_INSTANCE; ERROR_PIPE_BUSY (231).
    matches!(e.raw_os_error(), Some(5) | Some(231))
}

/// Conecta como cliente y verifica el propietario de la tubería.
pub async fn connect_client(name: &str) -> io::Result<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let client = loop {
        match ClientOptions::new().open(name) {
            Ok(c) => break c,
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    };
    verify_owner(&client)?;
    Ok(client)
}

fn verify_owner(client: &NamedPipeClient) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = HANDLE(client.as_raw_handle());
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `handle` es un handle de tubería abierto y válido mientras
    // `client` vive; la API reserva `sd` (liberado abajo) y `owner` apunta dentro.
    let rc: WIN32_ERROR = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    if rc.0 != 0 {
        return Err(io::Error::from_raw_os_error(rc.0 as i32));
    }
    let _sd = SecurityDescriptor(sd);
    let owner = sid_to_string(owner)?;
    let me = current_user_sid()?;
    if owner == me || owner == ADMINISTRATORS_SID {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "la tubería del agente pertenece a otro usuario ({owner}); posible suplantación"
            ),
        ))
    }
}
