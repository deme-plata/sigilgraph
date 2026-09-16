# Frames for the showcase (`sigil-vm-showcase.html`)

A frame is a PNG with an empty screen. Add it to `FRAMES` in `src/showcase.ts` with the pixel box of the
screen, measured on the image (scan from the centre outward to the first bright bezel pixel; inset 3–4 px).
Publish the PNG under `/home/orobit/sigilgraph-org-site/frames/`. Open `?frame=<key>`; press `k` for the
calibration box, arrows move it, shift+arrows resize, alt = 10 px steps, `c` copies `?screen=x,y,w,h`.

| key | file | image | screen box (x,y,w,h) |
|---|---|---|---|
| neon | frames/sigil-vm-bezel-neon.png | 1536×1024 | 168,119,1189,784 |
| multiverse (default) | frames/sigil-vm-multiverse.png | 1672×941 | 258,100,1166,712 — measured on the neon lines by saturation (measure.py --th picks up nebula; use the saturation pass in the skill) |
