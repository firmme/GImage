"""Generate a small, varied demo library for manual and visual testing."""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

OUT = Path(r"E:\code\GImage\samples\demo")
OUT.mkdir(parents=True, exist_ok=True)

PALETTES = [
    ((24, 34, 58), (255, 158, 84), "海边的清晨"),
    ((18, 48, 42), (140, 220, 160), "林间小路"),
    ((40, 24, 46), (232, 140, 200), "夜色霓虹"),
    ((20, 28, 36), (120, 180, 255), "雪山之巅"),
    ((44, 36, 22), (255, 210, 120), "沙漠落日"),
    ((16, 24, 28), (110, 220, 220), "潮汐"),
    ((36, 20, 22), (255, 120, 120), "红墙"),
    ((26, 30, 20), (190, 230, 120), "草地"),
    ((30, 26, 40), (180, 160, 255), "星空"),
    ((22, 32, 34), (255, 255, 255), "雾"),
    ((34, 28, 24), (230, 190, 160), "木屋"),
    ((20, 34, 30), (130, 255, 200), "极光"),
]

FONT_PATHS = [
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\segoeui.ttf",
]


def font(size: int) -> ImageFont.FreeTypeFont:
    for path in FONT_PATHS:
        if Path(path).exists():
            try:
                return ImageFont.truetype(path, size)
            except OSError:
                continue
    return ImageFont.load_default()


def gradient(size, top, bottom) -> Image.Image:
    width, height = size
    image = Image.new("RGB", size)
    draw = ImageDraw.Draw(image)
    for y in range(height):
        ratio = y / max(height - 1, 1)
        color = tuple(round(top[i] + (bottom[i] - top[i]) * ratio) for i in range(3))
        draw.line([(0, y), (width, y)], fill=color)
    return image


def decorate(image: Image.Image, title: str, subtitle: str) -> Image.Image:
    draw = ImageDraw.Draw(image, "RGBA")
    width, height = image.size
    draw.rounded_rectangle(
        [24, height - 140, min(width - 24, 24 + 360), height - 24],
        radius=18,
        fill=(0, 0, 0, 110),
    )
    draw.text((48, height - 122), title, font=font(34), fill=(255, 255, 255, 235))
    draw.text((48, height - 76), subtitle, font=font(20), fill=(255, 255, 255, 170))
    return image


def main() -> None:
    sizes = [
        (1600, 1067),
        (1200, 1800),
        (1920, 1080),
        (1000, 1000),
        (800, 1200),
        (2400, 1350),
        (640, 480),
        (1440, 900),
        (1080, 1440),
        (2048, 1365),
        (900, 1350),
        (1280, 720),
    ]
    formats = ["JPEG", "PNG", "WebP", "JPEG", "PNG", "JPEG", "BMP", "TIFF", "JPEG", "WebP", "PNG", "JPEG"]
    for index, ((size, fmt), (top, bottom, label)) in enumerate(zip(zip(sizes, formats), PALETTES)):
        image = gradient(size, top, bottom)
        image = decorate(image, label, f"GImage 示例 · {size[0]}×{size[1]} · {fmt}")
        name = f"{index + 1:02d}-{label}.{ {'JPEG': 'jpg', 'PNG': 'png', 'WebP': 'webp', 'BMP': 'bmp', 'TIFF': 'tif'}[fmt] }"
        image.save(OUT / name, fmt, quality=90)

    # Transparent PNG to verify alpha handling.
    alpha = Image.new("RGBA", (900, 900), (0, 0, 0, 0))
    draw = ImageDraw.Draw(alpha)
    draw.ellipse([80, 80, 820, 820], fill=(76, 141, 255, 235))
    draw.ellipse([260, 260, 640, 640], fill=(255, 255, 255, 180))
    draw.text((300, 430), "ALPHA", font=font(90), fill=(20, 30, 50, 255))
    alpha.save(OUT / "13-透明圆形.png", "PNG")

    # Animated GIF (first frame is shown in v1).
    frames = []
    for step in range(8):
        frame = Image.new("RGB", (640, 480), (20 + step * 12, 40, 90 + step * 8))
        draw = ImageDraw.Draw(frame)
        draw.text((40, 200), f"GIF frame {step + 1}", font=font(44), fill=(255, 255, 255))
        frames.append(frame)
    frames[0].save(OUT / "14-动画.gif", save_all=True, append_images=frames[1:], duration=120, loop=0)

    # A deliberately broken file: the grid must show a friendly placeholder.
    (OUT / "15-损坏.jpg").write_bytes(b"\xff\xd8\xff\xe0this is not a real jpeg")

    print(f"generated {len(list(OUT.iterdir()))} files in {OUT}")


if __name__ == "__main__":
    main()