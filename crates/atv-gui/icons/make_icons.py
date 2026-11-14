"""Draw the ATV app mark from the design handoff and save the icon files.

The mark (the design's "app-mark"): a dark rounded square, top #3b3b40 to
bottom #18181b, holding a 4 x 4 grid of white and empty cells, like a few
bits of an ATV frame. Run from anywhere: python make_icons.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent

# The cell pattern from the design, row by row (1 = white).
PATTERN = [
    [1, 0, 1, 1],
    [0, 1, 1, 0],
    [1, 1, 0, 1],
    [0, 0, 1, 1],
]


def draw(size: int) -> Image.Image:
    # Draw large and scale down, for smooth corners.
    big = size * 4
    image = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    gradient = Image.new("RGBA", (big, big))
    pixels = gradient.load()
    top, bottom = (0x3B, 0x3B, 0x40), (0x18, 0x18, 0x1B)
    for y in range(big):
        t = y / (big - 1)
        colour = tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,)
        for x in range(big):
            pixels[x, y] = colour
    mask = Image.new("L", (big, big), 0)
    margin = round(big * 0.06)
    ImageDraw.Draw(mask).rounded_rectangle(
        (margin, margin, big - margin, big - margin), radius=round(big * 0.23), fill=255
    )
    image.paste(gradient, (0, 0), mask)

    # The grid: cells of 6 units with gaps of 2.5, as in the 44 px About icon.
    unit = (big - 2 * margin) / 44
    cell, gap = 6 * unit, 2.5 * unit
    grid = 4 * cell + 3 * gap
    origin = (big - grid) / 2
    pen = ImageDraw.Draw(image)
    for row, values in enumerate(PATTERN):
        for column, on in enumerate(values):
            if on:
                x = origin + column * (cell + gap)
                y = origin + row * (cell + gap)
                pen.rectangle((x, y, x + cell, y + cell), fill=(255, 255, 255, 255))
    return image.resize((size, size), Image.LANCZOS)


def main() -> None:
    draw(32).save(HERE / "32x32.png")
    draw(128).save(HERE / "128x128.png")
    draw(256).save(HERE / "128x128@2x.png")
    draw(512).save(HERE / "icon.png")
    draw(256).save(HERE / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)])
    print("icons written to", HERE)


if __name__ == "__main__":
    main()
