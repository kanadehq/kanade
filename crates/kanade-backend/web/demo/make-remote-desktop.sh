#!/usr/bin/env bash
#
# Regenerate `demo/remote-desktop.jpg` — the synthetic desktop the demo's
# remote-view socket streams.
#
#   bash demo/make-remote-desktop.sh demo/remote-desktop.jpg
#
# Invoked through `bash`, not as `./…`, because nothing in this repository
# carries the executable bit — all nine shell scripts here are mode 100644,
# and there is not a single 100755 file in the tree. Marking this one would
# make it the lone exception in a repo developed on Windows, where the bit
# does not survive round-tripping anyway.
#
# Needs ImageMagick 7 (`magick`), which is NOT a dependency of this package:
# the output is committed, and this exists so the committed file can be
# reproduced or adjusted rather than being an unexplained binary.
#
# Deliberately generic. No vendor chrome, no logo, no imitation of any real
# product's UI, and above all not a capture of anyone's machine — the gallery
# page states outright that this desktop is drawn rather than photographed,
# and that has to stay true. The data shown (asset tags, lease dates, the
# people) matches the rest of the demo fixture so the two do not contradict
# each other on screen.
#
# Fonts are addressed by FILE PATH, not by family name: ImageMagick resolves
# a missing family to a default and still exits 0, so a name typo silently
# produces tofu instead of failing.
set -euo pipefail
OUT="${1:?usage: mkdesk.sh <out.jpg>}"
F=C:/Windows/Fonts/YuGothR.ttc
FB=C:/Windows/Fonts/YuGothB.ttc

magick -size 1920x1080 gradient:'#2b3a55'-'#16202f' \
  \( -size 1920x1080 xc:none \
     -fill '#ffffff10' -draw "polygon 0,760 1920,420 1920,1080 0,1080" \
     -fill '#ffffff08' -draw "polygon 0,930 1920,700 1920,1080 0,1080" \
  \) -composite \
  \
  `# ---- window 1: a spreadsheet-ish grid ----` \
  -fill '#0f172a40' -draw "roundrectangle 128,150 1010,600 10,10" \
  -fill '#f7f7f5'   -draw "roundrectangle 120,142 1002,592 10,10" \
  -fill '#e6e6e2'   -draw "roundrectangle 120,142 1002,182 10,10" \
  -fill '#3f4a5a' -font "$FB" -pointsize 20 -draw "text 140,171 '2026年度 資産台帳.xlsx'" \
  -fill '#c2c8d0' -draw "rectangle 120,182 1002,184" \
  -fill '#eef1f4' -draw "rectangle 120,190 1002,224" \
  -fill '#5b6472' -font "$F" -pointsize 16 \
    -draw "text 140,213 '管理番号'" -draw "text 320,213 '機種'" \
    -draw "text 600,213 '利用者'"   -draw "text 800,213 'リース満了'" \
  -fill '#d6dae0' \
    -draw "rectangle 120,224 1002,225" -draw "rectangle 120,266 1002,267" \
    -draw "rectangle 120,308 1002,309" -draw "rectangle 120,350 1002,351" \
    -draw "rectangle 120,392 1002,393" -draw "rectangle 120,434 1002,435" \
    -draw "rectangle 120,476 1002,477" -draw "rectangle 120,518 1002,520" \
    -draw "rectangle 310,190 311,520" -draw "rectangle 590,190 591,520" \
    -draw "rectangle 790,190 791,520" \
  -fill '#39414d' -font "$F" -pointsize 16 \
    -draw "text 140,252 'A-093089'" -draw "text 320,252 'ProDesk 400 G9'" -draw "text 600,252 '松本 沙織'"  -draw "text 800,252 '2027-03-31'" \
    -draw "text 140,294 'A-114522'" -draw "text 320,294 'ThinkPad L14'"   -draw "text 600,294 '小林 拓也'"  -draw "text 800,294 '2028-03-31'" \
    -draw "text 140,336 'A-097731'" -draw "text 320,336 'ProDesk 400 G9'" -draw "text 600,336 '井上 花子'"  -draw "text 800,336 '2027-03-31'" \
    -draw "text 140,378 'A-120044'" -draw "text 320,378 'Latitude 5440'"  -draw "text 600,378 '山本 大輔'"  -draw "text 800,378 '2029-03-31'" \
    -draw "text 140,420 'A-088410'" -draw "text 320,420 'EliteBook 640'"  -draw "text 600,420 '渡辺 大輔'"  -draw "text 800,420 '2028-03-31'" \
    -draw "text 140,462 'A-131265'" -draw "text 320,462 'ProDesk 400 G9'" -draw "text 600,462 '吉田 沙織'"  -draw "text 800,462 '2029-03-31'" \
  -fill '#8a93a0' -font "$F" -pointsize 14 -draw "text 140,552 '248 件中 6 件を表示'" \
  \
  `# ---- window 2: a console ----` \
  -fill '#0f172a40' -draw "roundrectangle 1058,300 1810,880 10,10" \
  -fill '#11161f'   -draw "roundrectangle 1050,292 1802,872 10,10" \
  -fill '#1c2431'   -draw "roundrectangle 1050,292 1802,330 10,10" \
  -fill '#cdd6e3' -font "$FB" -pointsize 18 -draw "text 1070,320 'ターミナル'" \
  -fill '#7ee787' -font "$F" -pointsize 16 \
    -draw "text 1070,372 'PS C:\\\\Users\\\\matsumoto48> kanade exec inventory-basic'" \
  -fill '#9aa7b8' -font "$F" -pointsize 16 \
    -draw "text 1070,402 'dispatched  request_id=2d5cfb3f'" \
    -draw "text 1070,428 'KANADE-PC-0001  ok    1.9s'" \
    -draw "text 1070,454 'KANADE-PC-0002  ok    2.1s'" \
    -draw "text 1070,480 'KANADE-PC-0004  ok    1.7s'" \
    -draw "text 1070,506 'KANADE-PC-0005  ok    2.4s'" \
  -fill '#f0883e' -font "$F" -pointsize 16 \
    -draw "text 1070,532 'KANADE-SV-0002  fail  exit=1'" \
  -fill '#9aa7b8' -font "$F" -pointsize 16 \
    -draw "text 1070,558 '5 hosts  4 ok  1 failed'" \
  -fill '#7ee787' -font "$F" -pointsize 16 -draw "text 1070,596 'PS C:\\\\Users\\\\matsumoto48> _'" \
  \
  `# ---- taskbar ----` \
  -fill '#0d1420' -draw "rectangle 0,1020 1920,1080" \
  -fill '#2f3d52' -draw "roundrectangle 16,1032 60,1068 6,6" \
  -fill '#3c4d66' -draw "roundrectangle 72,1032 116,1068 6,6" \
  -fill '#3c4d66' -draw "roundrectangle 128,1032 172,1068 6,6" \
  -fill '#3c4d66' -draw "roundrectangle 184,1032 228,1068 6,6" \
  -fill '#c8d2e0' -font "$F" -pointsize 18 -draw "text 1770,1057 '14:32'" \
  -quality 82 "$OUT"

magick identify -format "%wx%h %b\n" "$OUT"
