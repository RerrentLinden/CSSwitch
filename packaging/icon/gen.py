#!/usr/bin/env python3
"""生成 CSSwitch 图标的 SVG。用法:gen.py <输出目录>

  AppIcon.svg         主稿,用于 64 px 及以上
  AppIcon-small.svg   简化稿,用于 16 / 32 px(线更粗、开关更大、去掉普通碱基对)
  console-glyph.svg   控制台页眉与页签图标用的精简版:与简化稿同一组几何参数,
                      去掉滤镜与交叉处的遮挡,底板铺满画布

图形是按参数画出来的(正弦双链 + 交叉处上下遮挡),不是描摹任何现成图标。
改图形就改这里的参数,然后跑同目录的 build_icon.sh 重新生成 AppIcon.icns。
"""
import math, pathlib, sys

CX = CY = 512

PALETTE = dict(
    bg0="#3a332e", bg1="#171412", rim_hi="0.32", rim_lo="0.30",
    strand="#f6efe4", rung="#f6efe4",
    track0="#ff9d6e", track1="#dd5a2e", knob0="#ffffff", knob1="#efe6d8",
    shadow="0.36",
)

def path(points):
    return "M " + " L ".join(f"{x:.1f} {y:.1f}" for x, y in points)

def frame(p, body, extra_defs=""):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <defs>
    <linearGradient id="bg" gradientUnits="userSpaceOnUse" x1="0" y1="100" x2="0" y2="924">
      <stop offset="0" stop-color="{p['bg0']}"/><stop offset="1" stop-color="{p['bg1']}"/>
    </linearGradient>
    <linearGradient id="rim" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="{p['rim_hi']}"/>
      <stop offset="0.35" stop-color="#ffffff" stop-opacity="0.05"/>
      <stop offset="1" stop-color="#000000" stop-opacity="{p['rim_lo']}"/>
    </linearGradient>
    <linearGradient id="track" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{p['track0']}"/><stop offset="1" stop-color="{p['track1']}"/>
    </linearGradient>
    <linearGradient id="knob" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{p['knob0']}"/><stop offset="1" stop-color="{p['knob1']}"/>
    </linearGradient>
    <filter id="iconShadow" x="-20%" y="-20%" width="140%" height="150%">
      <feDropShadow dx="0" dy="14" stdDeviation="16" flood-color="#000000" flood-opacity="{p['shadow']}"/>
    </filter>
    <filter id="partShadow" x="-30%" y="-40%" width="160%" height="200%">
      <feDropShadow dx="0" dy="10" stdDeviation="12" flood-color="#000000" flood-opacity="0.30"/>
    </filter>
    <filter id="knobShadow" x="-40%" y="-40%" width="180%" height="200%">
      <feDropShadow dx="0" dy="6" stdDeviation="7" flood-color="#5a1e08" flood-opacity="0.45"/>
    </filter>
    <filter id="grain" x="0" y="0" width="100%" height="100%">
      <feTurbulence type="fractalNoise" baseFrequency="0.85" numOctaves="2" seed="7" result="n"/>
      <feColorMatrix type="matrix" values="0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0.9 -0.42"/>
    </filter>
    <clipPath id="plate"><rect x="100" y="100" width="824" height="824" rx="185"/></clipPath>
    {extra_defs}
  </defs>
  <g filter="url(#iconShadow)"><rect x="100" y="100" width="824" height="824" rx="185" fill="url(#bg)"/></g>
  <rect x="100" y="100" width="824" height="824" fill="#808080" opacity="0.10" filter="url(#grain)" clip-path="url(#plate)"/>
  <rect x="102" y="102" width="820" height="820" rx="183" fill="none" stroke="url(#rim)" stroke-width="4"/>
  {body}
</svg>
'''

class Helix:
    """双螺旋与开关的几何参数。small=True 是给 16 / 32 px 用的简化稿。"""
    def __init__(self, small):
        self.S = 60 if small else 36                      # 链的线宽
        self.A = 150 if small else 140                    # 振幅
        self.Y1, self.Y2 = 318, 706                       # 上下两个交叉点
        self.L = self.Y2 - self.Y1
        self.YT, self.YB = (188, 836) if small else (170, 854)
        self.lean = 42                                    # S 形中轴:上端偏左、下端偏右
        self.RW = self.S * 0.84                           # 碱基短横的线宽
        self.PW, self.PH = (236, 150) if small else (196, 104)   # 开关
        self.R = self.PH / 2

    def xc(self, y):
        return CX + self.lean * math.sin(math.pi * (y - CY) / 720)

    def half(self, y):
        return self.A * math.sin(math.pi * (y - self.Y1) / self.L)

    def strand(self, sign, step=2.0):
        n = int(round((self.YB - self.YT) / step))
        ys = [self.YT + (self.YB - self.YT) * i / n for i in range(n + 1)]
        return [(self.xc(y) + sign * self.half(y), y) for y in ys]

def comp_rung(p, small=False):
    """与 Science 图标同一种画法的双螺旋(粗线、上下各一个开口的交叉、中间一个大环、
    断开的碱基对短横、上端偏左下端偏右)。大环正中的那一级碱基对换成一个横放的拨动开关:
    两侧的短横照旧从链上伸出来,中间原本的断口由开关接上。"""
    h = Helix(small)
    S, A, RW = h.S, h.A, h.RW
    def rung(y, gap=44):
        w = abs(h.half(y)); x0 = h.xc(y)
        return (f'<path d="M {x0-w:.1f} {y} L {x0-gap/2:.1f} {y} M {x0+gap/2:.1f} {y} L {x0+w:.1f} {y}" fill="none" '
                f'stroke="{p["rung"]}" stroke-width="{RW:.1f}" stroke-linecap="round"/>')
    halo = S + 24
    def mask(mid, pts, yc):
        d = path([q for q in pts if abs(q[1] - yc) < 46])
        return (f'<mask id="{mid}" maskUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024">'
                f'<rect width="1024" height="1024" fill="#fff"/>'
                f'<path d="{d}" fill="none" stroke="#000" stroke-width="{halo}" stroke-linecap="butt"/></mask>')
    st = f'fill="none" stroke="{p["strand"]}" stroke-width="{S}" stroke-linecap="round" stroke-linejoin="round"'
    P, Q = h.strand(+1), h.strand(-1)
    # 开关:横放在大环最宽处,两端各留一小段碱基短横与链相连
    PW, PH, R = h.PW, h.PH, h.R
    x0 = h.xc(CY)
    knob_x = x0 + PW / 2 - R
    switch = f'''<path d="M {x0-A:.1f} {CY} L {x0-PW/2+8:.1f} {CY} M {x0+PW/2-8:.1f} {CY} L {x0+A:.1f} {CY}" fill="none" stroke="{p["rung"]}" stroke-width="{RW:.1f}" stroke-linecap="round"/>
  <g filter="url(#partShadow)"><rect x="{x0-PW/2:.1f}" y="{CY-R}" width="{PW}" height="{PH}" rx="{R}" fill="url(#track)"/></g>
  <rect x="{x0-PW/2+2:.1f}" y="{CY-R+2}" width="{PW-4}" height="{PH-4}" rx="{R-2}" fill="none" stroke="#ffffff" stroke-opacity="0.28" stroke-width="3"/>
  <circle cx="{knob_x:.1f}" cy="{CY}" r="{R-14}" fill="url(#knob)" filter="url(#knobShadow)"/>'''
    defs = mask("mP", Q, h.Y1) + mask("mQ", P, h.Y2)
    plain_rungs = "" if small else rung(222) + rung(398) + rung(626) + rung(802)
    body = f'''{plain_rungs}
  {switch}
  <path d="{path(P)}" {st} mask="url(#mP)"/>
  <path d="{path(Q)}" {st} mask="url(#mQ)"/>'''
    return frame(p, body, defs)

def console_glyph(p):
    """控制台用的精简版:简化稿的同一组链与开关,底板铺满画布(页签图标不需要留白与投影),
    不带滤镜与遮挡 —— 它要内联进单文件的 index.html,几十像素下这些细节也看不见。"""
    h = Helix(small=True)
    x0 = h.xc(CY)
    line = lambda pts: " ".join(f"{x:.0f},{y:.0f}" for x, y in pts)
    P, Q = h.strand(+1, step=18), h.strand(-1, step=18)
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="100 100 824 824">
<defs>
<linearGradient id="lg-bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{p['bg0']}"/><stop offset="1" stop-color="{p['bg1']}"/></linearGradient>
<linearGradient id="lg-track" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{p['track0']}"/><stop offset="1" stop-color="{p['track1']}"/></linearGradient>
</defs>
<rect x="100" y="100" width="824" height="824" rx="185" fill="url(#lg-bg)"/>
<g fill="none" stroke="{p['strand']}" stroke-linecap="round" stroke-linejoin="round">
<path stroke-width="{h.RW:.0f}" d="M{x0-h.A:.0f} {CY}H{x0-h.PW/2+8:.0f}M{x0+h.PW/2-8:.0f} {CY}H{x0+h.A:.0f}"/>
<polyline stroke-width="{h.S}" points="{line(P)}"/>
<polyline stroke-width="{h.S}" points="{line(Q)}"/>
</g>
<rect x="{x0-h.PW/2:.0f}" y="{CY-h.R:.0f}" width="{h.PW}" height="{h.PH}" rx="{h.R:.0f}" fill="url(#lg-track)"/>
<circle cx="{x0+h.PW/2-h.R:.0f}" cy="{CY}" r="{h.R-14:.0f}" fill="{p['knob0']}"/>
</svg>
'''

out = pathlib.Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
(out / "AppIcon.svg").write_text(comp_rung(PALETTE))
(out / "AppIcon-small.svg").write_text(comp_rung(PALETTE, small=True))
(out / "console-glyph.svg").write_text(console_glyph(PALETTE))
print("written:", sorted(f.name for f in out.glob("*.svg")))
