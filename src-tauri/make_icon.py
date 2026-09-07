"""Gera o icone-fonte do tkdrama (1024x1024).

A marca: o quadro vertical 9:16 da novela, com a seta de download descendo
dentro dele. Precisa continuar legivel a 32px na barra de tarefas, entao e
so isso - sem detalhe que some quando encolhe.

Uso: py make_icon.py && npm run tauri icon src-tauri/icon-source.png
"""
from PIL import Image, ImageDraw

S = 1024
SS = 4  # supersampling para bordas suaves
BG = (22, 24, 27, 255)       # grafite do app
EDGE = (41, 45, 51, 255)     # borda sutil
AMBER = (240, 168, 48, 255)  # o mesmo acento da interface

img = Image.new("RGBA", (S * SS, S * SS), (0, 0, 0, 0))
d = ImageDraw.Draw(img)


def px(v: float) -> float:
    return v * SS


# Fundo: quadrado arredondado no raio do Windows 11.
d.rounded_rectangle(
    [px(0), px(0), px(S), px(S)],
    radius=px(180),
    fill=BG,
    outline=EDGE,
    width=int(px(6)),
)

# Quadro vertical 9:16, centralizado.
fw, fh = 300, 534
fx, fy = (S - fw) / 2, (S - fh) / 2
d.rounded_rectangle(
    [px(fx), px(fy), px(fx + fw), px(fy + fh)],
    radius=px(38),
    outline=AMBER,
    width=int(px(30)),
)

# Seta descendo dentro do quadro.
cx = S / 2
shaft_top, shaft_bottom = fy + 118, fy + 330
d.rounded_rectangle(
    [px(cx - 24), px(shaft_top), px(cx + 24), px(shaft_bottom)],
    radius=px(24),
    fill=AMBER,
)
head = 82
d.polygon(
    [
        (px(cx - head), px(shaft_bottom - 16)),
        (px(cx + head), px(shaft_bottom - 16)),
        (px(cx), px(shaft_bottom + 104)),
    ],
    fill=AMBER,
)

# Base: o disco onde o episodio pousa.
d.rounded_rectangle(
    [px(cx - 96), px(fy + fh - 96), px(cx + 96), px(fy + fh - 66)],
    radius=px(15),
    fill=AMBER,
)

img.resize((S, S), Image.LANCZOS).save("icon-source.png")
print("icon-source.png gerado")
