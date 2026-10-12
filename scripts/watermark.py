"""Gera a marca d'agua "Noveleiras de plantao" (PNG transparente) e a logo do
cartao final.

No mesmo espirito de src-tauri/make_icon.py: a tipografia e decidida aqui, uma
vez, e commitada como PNG. O pipeline de cortes (cortes.rs) so faz overlay do
arquivo pronto, entao a marca sai identica entre um clipe e outro, sem
recalcular fonte a cada render.

A marca e um pill escuro translucido com a FLOR do app a esquerda e o nome a
direita, em Montserrat SemiBold (fonte OFL commitada em src-tauri/branding/fonts,
a mesma que o gancho e o cartao final usam — uma so familia tipografica no clipe).
Altura do conjunto ~5% do quadro de 1080x1920 (~96px): legivel no feed sem
dominar a cena. O pipeline a posiciona no centro horizontal, no topo, dentro da
zona segura das interfaces verticais (TikTok/Reels/Shorts cobrem o rodape com
legenda e a lateral direita com os botoes; o topo abaixo das abas fica livre).

Tambem grava a flor em 480px (branding/logo-card.png) para o cartao final: a
logo original tem 1254px e nao ha por que embutir 1,2 MB no binario.

Uso: py scripts/watermark.py   (grava src-tauri/watermark.png e branding/logo-card.png)
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
FONT_PATH = ROOT / "src-tauri" / "branding" / "fonts" / "Montserrat-SemiBold.ttf"
LOGO_PATH = ROOT / "src-tauri" / "branding" / "logo-app.png"

# --- Parametros presos a especificacao (quadro de saida 1080x1920) -----------
FRAME_H = 1920
PILL_H = round(FRAME_H * 0.05)       # ~96px: altura do conjunto = 5% do quadro
SS = 4                               # supersampling para bordas suaves
TEXT = "Noveleiras de plantão"

PILL_RGB = (16, 14, 18)              # grafite quase preto
PILL_ALPHA = round(255 * 0.55)       # pill translucido, mas presente
TEXT_RGB = (255, 255, 255)
TEXT_ALPHA = round(255 * 0.96)
PAD_X = round(PILL_H * 0.30)         # respiro horizontal dentro do pill
GAP = round(PILL_H * 0.16)           # espaco entre a flor e o texto
LOGO_FRAC = 0.78                     # altura da flor em relacao ao pill
LOGO_CARD_PX = 480


def load_font(px: int) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(str(FONT_PATH), px)
    except OSError as e:
        raise SystemExit(f"Nao consegui carregar a fonte em {FONT_PATH}: {e}")


def main() -> None:
    if not LOGO_PATH.is_file():
        raise SystemExit(f"logo do app nao encontrada em {LOGO_PATH}")

    pill_h = PILL_H * SS
    font = load_font(round(pill_h * 0.40))
    pad_x = PAD_X * SS
    gap = GAP * SS

    probe = Image.new("RGBA", (1, 1))
    box = ImageDraw.Draw(probe).textbbox((0, 0), TEXT, font=font)
    text_w = box[2] - box[0]
    text_h = box[3] - box[1]

    logo_side = round(pill_h * LOGO_FRAC)
    logo = Image.open(LOGO_PATH).convert("RGBA").resize((logo_side, logo_side), Image.LANCZOS)

    pill_w = pad_x + logo_side + gap + text_w + pad_x
    img = Image.new("RGBA", (pill_w, pill_h), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(
        [0, 0, pill_w - 1, pill_h - 1],
        radius=pill_h // 2,
        fill=(*PILL_RGB, PILL_ALPHA),
    )
    img.alpha_composite(logo, (pad_x, (pill_h - logo_side) // 2))

    tx = pad_x + logo_side + gap - box[0]
    ty = (pill_h - text_h) // 2 - box[1]
    d = ImageDraw.Draw(img)
    d.text((tx, ty), TEXT, font=font, fill=(*TEXT_RGB, TEXT_ALPHA))

    out_w = round(pill_w / SS)
    out_h = round(pill_h / SS)
    img = img.resize((out_w, out_h), Image.LANCZOS)
    dest = ROOT / "src-tauri" / "watermark.png"
    img.save(dest)
    print(f"{dest} gerado ({out_w}x{out_h})")

    card = Image.open(LOGO_PATH).convert("RGBA").resize((LOGO_CARD_PX, LOGO_CARD_PX), Image.LANCZOS)
    card_dest = ROOT / "src-tauri" / "branding" / "logo-card.png"
    card.save(card_dest)
    print(f"{card_dest} gerado ({LOGO_CARD_PX}x{LOGO_CARD_PX})")


if __name__ == "__main__":
    main()
