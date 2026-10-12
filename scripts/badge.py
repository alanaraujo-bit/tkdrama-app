"""Prepara o selo da estrategia 'cobrir' a partir da logo do app.

Mesmo espirito de scripts/watermark.py: a arte e decidida uma vez e commitada como
PNG; o pipeline de cortes (cortes.rs) so escala e faz overlay do arquivo pronto.

A marca embutida que a gente cobre (ex.: o "R" do ReelShort) e um SELO QUADRADO. O
nosso selo e a LOGO DO APP (a flor de "Noveleiras de Plantao"), commitada em
branding/logo-app.png. A logo tem silhueta irregular e fundo transparente; se
sobreposta crua, o "R" quadrado poderia vazar pelos vaos entre as petalas. Por isso
o selo e a logo sobre uma PLACA quadrada de cantos arredondados, opaca, que tampa o
"R" por completo — a placa cobre, a flor identifica. Um respiro (padding) deixa a
flor "dentro" da placa, com folga nas bordas.

Uso: py scripts/badge.py   (grava src-tauri/branding/badge-logo.png)
"""
from pathlib import Path
from PIL import Image, ImageDraw

SIDE = 256                       # lado do PNG commitado (o pipeline reescala ao alvo)
SS = 4                           # supersampling para bordas suaves
RADIUS_FRAC = 0.24               # cantos arredondados: 24% do lado (forma de selo)
PAD_FRAC = 0.12                  # respiro entre a placa e a flor
# Placa: creme quente combinando com a flor, quase opaca (tem que TAMPAR o "R").
PLATE_RGB = (250, 244, 240)
PLATE_ALPHA = round(255 * 0.96)
# Aro sutil para o selo nao sumir em fundo claro.
RING_RGB = (214, 51, 92)         # rosa da flor
RING_ALPHA = round(255 * 0.55)
RING_W_FRAC = 0.035


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    src = root / "src-tauri" / "branding" / "logo-app.png"
    if not src.is_file():
        raise SystemExit(f"logo do app nao encontrada em {src}")

    side = SIDE * SS
    radius = round(side * RADIUS_FRAC)
    img = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # Placa opaca de cantos arredondados.
    d.rounded_rectangle([0, 0, side - 1, side - 1], radius=radius, fill=(*PLATE_RGB, PLATE_ALPHA))

    # Flor centralizada, com respiro.
    pad = round(side * PAD_FRAC)
    inner = side - 2 * pad
    logo = Image.open(src).convert("RGBA").resize((inner, inner), Image.LANCZOS)
    img.alpha_composite(logo, (pad, pad))

    # Aro por cima, para definir a borda do selo.
    ring_w = max(1, round(side * RING_W_FRAC))
    inset = ring_w // 2
    d.rounded_rectangle(
        [inset, inset, side - 1 - inset, side - 1 - inset],
        radius=radius - inset,
        outline=(*RING_RGB, RING_ALPHA),
        width=ring_w,
    )

    img = img.resize((SIDE, SIDE), Image.LANCZOS)
    dest = root / "src-tauri" / "branding" / "badge-logo.png"
    dest.parent.mkdir(parents=True, exist_ok=True)
    img.save(dest)
    print(f"{dest} gerado ({SIDE}x{SIDE}) a partir de {src.name}")


if __name__ == "__main__":
    main()
