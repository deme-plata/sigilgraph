# Shadows in the Chain — Director's Cut Trailer (storyboard + render prompts)
Engine: Claude Code (pre-production). Render target: HunyuanVideo 1.5 (~75s/clip @ 4090) or LTX-2.3 (4K+audio). Each SHOT = a paste-ready text-to-video prompt. ~75s/clip → 8 shots ≈ a Phase-2 GPU-hour.

**Tone:** neon-noir cyberpunk, rain-slick Copenhagen-meets-Neo-Tokyo, violet/cyan SIGIL glow, BLAKE3 hashes drifting like ash. Sigil-gold accents on every "verified" beat.

— SHOT 1 (0:00–0:04) · COLD OPEN —
"Slow push through a rain-soaked neon alley at night, cyan and violet signage reflecting in puddles, a lone figure in a long coat walking away from camera, volumetric fog, cinematic anamorphic, 35mm, moody."

— SHOT 2 (0:04–0:08) · THE CHAIN —
"Macro: glowing hexagonal blocks linking into an endless luminous chain floating in dark space, violet energy threading between them, data particles, slow dolly, hyper-detailed, depth of field."

— SHOT 3 (0:08–0:12) · THE PROTAGONIST —
"Close-up of a hacker's face lit by a holographic terminal, cyan code reflecting in their eyes, tense, breath visible, neon rim light, shallow focus, film grain."

— SHOT 4 (0:12–0:16) · THE HEIST —
"First-person sprint through a server cathedral, towering racks pulsing with light, alarms strobing red, motion blur, fast tracking shot, cinematic."

— SHOT 5 (0:16–0:20) · STATE ROOTS —
"Four glowing Merkle-tree roots crystallizing in mid-air around the protagonist, gold light verifying each, '10ms' flashing, slow motion, awe."

— SHOT 6 (0:20–0:24) · BETRAYAL —
"Two silhouettes face off on a rooftop under acid rain, city of light below, one draws a glowing blade, dramatic backlight, tension, wide cinematic."

— SHOT 7 (0:24–0:28) · THE VERIFY GATE —
"A vast vault door made of light seals shut as a gold SIGIL sigil locks into place, particles, triumphant, slow push-in."

— SHOT 8 (0:28–0:32) · TITLE —
"Black screen, a single gold sigil ignites then resolves into the title 'SHADOWS IN THE CHAIN', violet glow, embers, cinematic logo reveal."

## Render recipe (Phase 2, when a GPU box is up)
1. vLLM-free: a dedicated diffusion box (RTX 4090 ~$0.4/hr or A100), pip install the model (HunyuanVideo 1.5 / LTX-2.3 / Wan 2.2).
2. Loop the 8 prompts → 8 clips (~75s each on a 4090).
3. Concatenate (ffmpeg) + LTX-2.3 native audio OR a score → the director's cut.
HONEST: this needs a Phase-2 GPU + ~1 GPU-hour. Not 100s, not via the (unconnected) HF MCP — but the prompts are ready to render the moment the budget is there.
