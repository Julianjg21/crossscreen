# ADR-0004: Emparejamiento SPAKE2 y TLS mutuo con certificados fijados

**Estado:** Propuesta (validar con la PoC-9) · **Fecha:** 2026-11-08

## Contexto
Estado de las referencias:

| Proyecto | Emparejamiento y cifrado |
|---|---|
| Weylus | Sin cifrado; código de acceso en la URL |
| Deskreen | Señalización en claro; las funciones de "cifrado" son la identidad; vulnerable a MITM |
| Sunshine | PIN de 4 dígitos con AES-128-ECB, atacable offline; vídeo sin cifrar en LAN por defecto |
| ScreenExtend | OTP correcto, pero HTTPS sin fijar el certificado |
| TwinDesk | El mejor: certificado fijado + SAS por commit/reveal |

Un monitor transmite todo lo que el usuario ve (contraseñas, correo), así que la confidencialidad es crítica también en la LAN.

## Decisión
1. **Identidad por dispositivo:** clave Ed25519 con un certificado X.509 autofirmado (`rcgen`). La clave privada se guarda en el almacén del SO.
2. **Emparejamiento:**
   1. el receptor muestra un PIN de 6 dígitos, válido 2 minutos;
   2. ambos ejecutan **SPAKE2** (crate `spake2`) dentro de un stream QUIC provisional, autenticado solo a nivel de transporte;
   3. la clave resultante autentica, mediante un MAC, el intercambio de las huellas SHA-256 de los certificados;
   4. ambos guardan la huella del otro.

   Límites: 5 intentos por ventana y *backoff* exponencial.
3. **Conexiones normales:** TLS 1.3 mutuo en QUIC con un `ServerCertVerifier`/`ClientCertVerifier` de rustls que **solo acepta huellas fijadas**.
4. **Todo el tráfico** (vídeo incluido) viaja dentro de la conexión QUIC cifrada. No existe modo sin cifrar.
5. **Revocación:** "Olvidar dispositivo" borra la huella en local y lo notifica al par si está conectado.

## Alternativas
- **SAS sin PAKE (TwinDesk).** Es seguro, pero exige comparar el código en las dos pantallas y depende de que el usuario lo verifique. Con SPAKE2 basta con teclearlo una vez.
- **Noise XX + SAS.** Válido, pero duplicaría la pila criptográfica junto a TLS. Queda como plan B.
- **Confianza en el primer uso (TOFU) sin PIN.** Vulnerable a MITM en el primer contacto. Se descarta.

## Consecuencias
- Un PIN de 6 dígitos con PAKE limita al atacante a un intento online por sesión: probabilidad ≤ 5·10⁻⁶ con 5 intentos.
- Requiere un verificador rustls personalizado, que se cubre con tests negativos.
