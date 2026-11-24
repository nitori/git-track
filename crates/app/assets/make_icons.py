"""Renders the git-track icons (multi-size .ico files) used as Windows resources.

Run from this directory:  uv run --with pillow make_icons.py
"""

from PIL import Image, ImageChops, ImageDraw

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SUPERSAMPLE = 8

TOP = (107, 156, 247)  # matches the app accent colour
BOTTOM = (47, 95, 196)
WHITE = (255, 255, 255, 255)
BADGE = (242, 107, 122, 255)  # matches the app danger colour


def bezier(p0, p1, p2, steps=48):
    points = []
    for i in range(steps + 1):
        t = i / steps
        x = (1 - t) ** 2 * p0[0] + 2 * (1 - t) * t * p1[0] + t**2 * p2[0]
        y = (1 - t) ** 2 * p0[1] + 2 * (1 - t) * t * p1[1] + t**2 * p2[1]
        points.append((x, y))
    return points


def render(size: int, alert: bool) -> Image.Image:
    n = size * SUPERSAMPLE

    def u(v: float) -> float:
        return v * n

    # Rounded square with a vertical gradient.
    gradient = Image.new("RGBA", (1, n))
    for y in range(n):
        t = y / (n - 1)
        gradient.putpixel(
            (0, y), tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,)
        )
    gradient = gradient.resize((n, n))
    mask = Image.new("L", (n, n), 0)
    margin = u(0.04)
    ImageDraw.Draw(mask).rounded_rectangle(
        (margin, margin, n - margin, n - margin), radius=u(0.22), fill=255
    )
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    img.paste(gradient, (0, 0), mask)

    draw = ImageDraw.Draw(img)
    # Thicker strokes for small sizes so they survive downsampling.
    stroke = max(u(0.095), 1.7 * SUPERSAMPLE)
    node = max(u(0.105), 2.2 * SUPERSAMPLE)

    # Main line with two commits.
    x_main = u(0.34)
    top_y, bottom_y = u(0.27), u(0.75)
    draw.line((x_main, top_y, x_main, bottom_y), fill=WHITE, width=round(stroke))
    for y in (top_y, bottom_y):
        draw.ellipse((x_main - node, y - node, x_main + node, y + node), fill=WHITE)

    # Branch curving out of the main line and ending in an up arrow (= push).
    x_branch = u(0.67)
    head_tip, head_base = u(0.17), u(0.40)
    curve = bezier((x_main, u(0.68)), (x_branch, u(0.66)), (x_branch, head_base))
    draw.line(curve, fill=WHITE, width=round(stroke), joint="curve")
    half = u(0.17)
    draw.polygon(
        [(x_branch, head_tip), (x_branch - half, head_base + u(0.02)), (x_branch + half, head_base + u(0.02))],
        fill=WHITE,
    )

    if alert:
        cx, cy, r = u(0.76), u(0.76), u(0.235)
        gap = max(u(0.06), 1.0 * SUPERSAMPLE)
        cut = Image.new("L", (n, n), 255)
        ImageDraw.Draw(cut).ellipse((cx - r - gap, cy - r - gap, cx + r + gap, cy + r + gap), fill=0)
        img.putalpha(ImageChops.multiply(img.getchannel("A"), cut))
        ImageDraw.Draw(img).ellipse((cx - r, cy - r, cx + r, cy + r), fill=BADGE)

    return img.resize((size, size), Image.Resampling.LANCZOS)


def save(path: str, alert: bool) -> None:
    images = [render(s, alert) for s in SIZES]
    images[-1].save(path, format="ICO", sizes=[(s, s) for s in SIZES], append_images=images[:-1])


def preview(path: str) -> None:
    """Small sizes on a dark and a light strip, scaled up 4x for inspection."""
    cells = [(16, False), (16, True), (24, False), (24, True), (32, False), (32, True), (48, False), (48, True)]
    width = sum(s + 8 for s, _ in cells) + 8
    sheet = Image.new("RGBA", (width, 2 * 56), (0, 0, 0, 0))
    for row, bg in enumerate([(32, 32, 32, 255), (238, 238, 238, 255)]):
        ImageDraw.Draw(sheet).rectangle((0, row * 56, width, row * 56 + 55), fill=bg)
        x = 8
        for s, alert in cells:
            icon = render(s, alert)
            sheet.alpha_composite(icon, (x, row * 56 + (56 - s) // 2))
            x += s + 8
    sheet = sheet.resize((sheet.width * 4, sheet.height * 4), Image.Resampling.NEAREST)
    sheet.save(path)


if __name__ == "__main__":
    import sys

    save("git-track.ico", alert=False)
    save("git-track-alert.ico", alert=True)
    if len(sys.argv) > 1:
        preview(sys.argv[1])
