"""Cut Axel out of a picture with a painted-in checkerboard, and make every site asset from it."""

from PIL import Image, ImageFilter, ImageDraw
from collections import deque
SAT, LIGHT, MIN_AREA = 50, 140, 60  # light, barely tinted pixels; enclosed patches must also pass is_checker
src = Image.open('assets/axel-mascot-source.png').convert('RGB')
W, H = src.size; px = src.load()
def is_bg(p):
    r, g, b = p
    return max(r, g, b) - min(r, g, b) < SAT and min(r, g, b) > LIGHT
def is_checker(comp):
    """A patch of the painted checkerboard, not a highlight of the art: touching the
    border, or big, nearly colourless, and made of both its tones (white and grey)."""
    if any(j % W in (0, W - 1) or j // W in (0, H - 1) for j in comp):
        return True
    if len(comp) < MIN_AREA:
        return False
    pts = [px[j % W, j // W] for j in comp]
    sat = sum(max(p) - min(p) for p in pts) / len(pts)
    white = sum(1 for p in pts if min(p) > 238) / len(pts)
    grey = sum(1 for p in pts if 185 < max(p) < 232) / len(pts)
    return sat < 14 and white > 0.2 and grey > 0.2

label = bytearray(W * H)
for y0 in range(H):
    for x0 in range(W):
        i0 = y0 * W + x0
        if label[i0] or not is_bg(px[x0, y0]): continue
        comp = []; q = deque([(x0, y0)]); label[i0] = 3
        while q:
            x, y = q.popleft(); comp.append(y * W + x)
            for nx, ny in ((x-1,y),(x+1,y),(x,y-1),(x,y+1)):
                if 0 <= nx < W and 0 <= ny < H:
                    j = ny * W + nx
                    if not label[j] and is_bg(px[nx, ny]): label[j] = 3; q.append((nx, ny))
        for j in comp: label[j] = 1 if is_checker(comp) else 2
mask = Image.frombytes('L', (W, H), bytes(0 if l == 1 else 255 for l in label))
mask = mask.filter(ImageFilter.MinFilter(5)).filter(ImageFilter.GaussianBlur(1.2))
out = src.convert('RGBA'); out.putalpha(mask)
bbox = mask.point(lambda v: 255 if v > 8 else 0).getbbox()
m = out.crop(bbox); m.save('assets/axel-mascot.png', optimize=True)
INK = (15, 18, 16, 255)
m.save('public/mascot.webp', 'WEBP', quality=90, method=6)
def tile(size, pad=0.08, round_=True):
    c = Image.new('RGBA', (size, size), INK); inner = int(size * (1 - 2 * pad)); s = inner / max(m.size)
    mm = m.resize((int(m.width * s), int(m.height * s)), Image.LANCZOS)
    c.alpha_composite(mm, ((size - mm.width) // 2, (size - mm.height) // 2))
    if round_:
        k = Image.new('L', (size, size), 0); ImageDraw.Draw(k).rounded_rectangle((0, 0, size - 1, size - 1), radius=int(size * 0.225), fill=255); c.putalpha(k)
    return c
# The favicons (src/app/icon.png, apple-icon.png, favicon.ico) come from axel-icon-source.png.
tile(256).save('public/mascot-icon.png', optimize=True)
og = Image.new('RGBA', (1200, 630), INK); h = 540; mm = m.resize((int(m.width * h / m.height), h), Image.LANCZOS)
og.alpha_composite(mm, ((1200 - mm.width) // 2, (630 - h) // 2)); og.convert('RGB').save('public/og.png', optimize=True)
print(m.size)
