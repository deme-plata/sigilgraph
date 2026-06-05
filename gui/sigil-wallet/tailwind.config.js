/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        // SIGIL palette — obsidian + violet + provenance gold.
        // `quantum` namespace KEPT for backward-compat with 117 existing components;
        // values RECOLORED to SIGIL identity (Phase B 0.2.0).
        // New SIGIL-named tokens live alongside; new code should prefer `sigil-*`.
        quantum: {
          dark:    '#0a0a0f', // obsidian (was #0A0B14)
          darker:  '#050510', // deep obsidian
          indigo:  '#1a1428', // panel violet-tinted (was #1A1B26 cool-grey)
          violet:  '#4c1d95', // deep violet (was #2D1B69 dim)
          purple:  '#8b5cf6', // SIGIL accent (was #6B46C1)
          cyan:    '#c084fc', // SIGIL accent-bright (was #00D9FF; cyan→bright violet)
          green:   '#4ade80', // ok state (was #00FF88, softer)
          pink:    '#f472b6', // accent pink (was #FF0080, softer)
          yellow:  '#fbbf24', // provenance gold (was #FFD700; reserved for .proof badges)
          blue:    '#c084fc', // alias to accent-bright (was #0080FF; blue→violet)
        },
        sigil: {
          obsidian:    '#0a0a0f',
          panel:       '#1a1428',
          accent:      '#8b5cf6',
          'accent-bright': '#c084fc',
          gold:        '#fbbf24',
          text:        '#e2e8f0',
          muted:       '#94a3b8',
          ok:          '#4ade80',
          warn:        '#fb923c',
          danger:      '#f43f5e',
        },
      },
      animation: {
        'quantum-pulse': 'quantum-pulse 2s ease-in-out infinite',
        'photon-flow': 'photon-flow 3s linear infinite',
        'entangle': 'entangle 4s ease-in-out infinite',
        'collapse': 'collapse 0.5s ease-out',
        'fractal-bloom': 'fractal-bloom 1s ease-out',
        'rainbow-shift': 'rainbow-shift 5s linear infinite',
      },
      keyframes: {
        'quantum-pulse': {
          '0%, 100%': { opacity: 0.4, transform: 'scale(1)' },
          '50%': { opacity: 1, transform: 'scale(1.05)' },
        },
        'photon-flow': {
          '0%': { transform: 'translateY(-100%)' },
          '100%': { transform: 'translateY(100%)' },
        },
        'entangle': {
          '0%, 100%': { transform: 'rotate(0deg) scale(1)' },
          '50%': { transform: 'rotate(180deg) scale(1.1)' },
        },
        'collapse': {
          '0%': { filter: 'blur(10px)', opacity: 0.3 },
          '100%': { filter: 'blur(0px)', opacity: 1 },
        },
        'fractal-bloom': {
          '0%': { transform: 'scale(0) rotate(0deg)' },
          '100%': { transform: 'scale(1) rotate(360deg)' },
        },
        'rainbow-shift': {
          '0%': { filter: 'hue-rotate(0deg)' },
          '100%': { filter: 'hue-rotate(360deg)' },
        }
      },
      backgroundImage: {
        // SIGIL palette gradients (Phase B 0.2.0)
        // Names kept for backward-compat; new values reflect obsidian/violet identity
        'quantum-gradient': 'linear-gradient(135deg, #4c1d95 0%, #6d28d9 25%, #8b5cf6 50%, #c084fc 75%, #fbbf24 100%)',
        'photon-gradient':  'linear-gradient(180deg, transparent, rgba(192, 132, 252, 0.4), transparent)',
        'entanglement':     'radial-gradient(circle, rgba(139, 92, 246, 0.35) 0%, transparent 70%)',
        'sigil-glow':       'radial-gradient(ellipse at 30% 0%, rgba(139, 92, 246, 0.18) 0%, transparent 55%)',
        'sigil-provenance': 'linear-gradient(135deg, #fbbf24 0%, #f59e0b 100%)',
      },
      fontFamily: {
        // Phase B 0.2.1 — fonts: mono for headings, sans for body (no Inter)
        'sigil-mono': ['"JetBrains Mono"', 'ui-monospace', 'SFMono-Regular', 'monospace'],
        'sigil-sans': ['"IBM Plex Sans"', 'system-ui', '-apple-system', 'sans-serif'],
      },
      boxShadow: {
        // Phase B 0.2.3 — sigil glow (drop-shadow for buttons / balance)
        'sigil':        '0 0 24px rgba(139, 92, 246, 0.45), 0 0 48px rgba(139, 92, 246, 0.15)',
        'sigil-strong': '0 0 32px rgba(192, 132, 252, 0.6), 0 0 64px rgba(192, 132, 252, 0.25)',
        'sigil-gold':   '0 0 20px rgba(251, 191, 36, 0.5)',
      }
    },
  },
  plugins: [],
}