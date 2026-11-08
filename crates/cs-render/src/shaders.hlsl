// Shaders del visor (D3D11, SM 4.0). Se compilan en tiempo de ejecución con
// D3DCompile (d3dcompiler_47.dll, incluida en Windows 10/11).

cbuffer Params : register(b0)
{
    float4 video_rect;  // x0, y0, x1, y1 en NDC
    float2 uv_scale;    // tamaño visible / tamaño de la superficie
    float2 pad0;
    float4 cursor_rect; // x0, y0, x1, y1 en NDC
};

// Vistas sobre la MISMA textura NV12 del decodificador (una rebanada del array).
Texture2DArray<float>  tex_y  : register(t0);
Texture2DArray<float2> tex_uv : register(t1);
// Forma real del cursor (RGBA premultiplicado), subida desde `CursorShape`.
Texture2D<float4>      tex_cursor : register(t2);
SamplerState samp : register(s0);

struct VsOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

// Quad como triangle strip de 4 vértices sin vertex buffer.
VsOut quad(uint id, float4 rect, float2 scale)
{
    float2 t = float2(id & 1, id >> 1);
    VsOut o;
    o.pos = float4(lerp(rect.xy, rect.zw, t), 0.0, 1.0);
    o.uv = t * scale;
    return o;
}

VsOut vs_video(uint id : SV_VertexID) { return quad(id, video_rect, uv_scale); }
VsOut vs_cursor(uint id : SV_VertexID) { return quad(id, cursor_rect, float2(1.0, 1.0)); }

// NV12 BT.709 rango limitado -> RGB.
float4 ps_video(VsOut i) : SV_Target
{
    float y = tex_y.Sample(samp, float3(i.uv, 0.0));
    float2 uv = tex_uv.Sample(samp, float3(i.uv, 0.0));
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    uv = (uv - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb;
    rgb.r = y + 1.5748 * uv.y;
    rgb.g = y - 0.1873 * uv.x - 0.4681 * uv.y;
    rgb.b = y + 1.8556 * uv.x;
    return float4(saturate(rgb), 1.0);
}

// Flecha procedimental: triángulo (0,0)-(0,1)-(0.7,0.75) con borde negro.
float inside_arrow(float2 p, float grow)
{
    // Semiplanos: x >= -grow, y <= 1 + grow, y en la recta superior/inferior.
    float e0 = p.x + grow;
    float e1 = (0.75 * p.x - 0.7 * p.y) * -1.0 + grow;          // borde (0,0)-(0.7,0.75)
    float e2 = (0.25 * p.x + 0.7 * (p.y - 1.0)) * -1.0 + grow;  // borde (0.7,0.75)-(0,1)
    return (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) ? 1.0 : 0.0;
}

float4 ps_cursor(VsOut i) : SV_Target
{
    float outer = inside_arrow(i.uv, 0.06);
    float inner = inside_arrow(i.uv, 0.0);
    if (outer < 0.5) discard;
    return inner > 0.5 ? float4(1.0, 1.0, 1.0, 1.0) : float4(0.0, 0.0, 0.0, 1.0);
}

// Cursor real: textura RGBA con alfa premultiplicado (mezcla ONE / INV_SRC_ALPHA).
float4 ps_cursor_tex(VsOut i) : SV_Target
{
    float4 c = tex_cursor.Sample(samp, i.uv);
    if (c.a <= 0.0) discard;
    return c;
}
