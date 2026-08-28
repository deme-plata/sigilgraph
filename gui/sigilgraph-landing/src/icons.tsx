// Small inline SVG icon set — deliberately NOT emoji. Color-emoji glyphs
// depend on a system emoji font being installed; plenty of Linux desktops
// (and the headless renderer used to verify this page) don't have one,
// so emoji silently render as empty boxes. Plain SVG renders identically
// everywhere, no font dependency.
import type { SVGProps } from 'react';

const base = { width: 22, height: 22, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: 1.7, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const };

export const IconSigil = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><path d="M12 2 L21 7 L21 17 L12 22 L3 17 L3 7 Z" /><path d="M12 2 V22 M3 7 L21 17 M21 7 L3 17" /></svg>
);
export const IconCopy = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={14} height={14} {...p}><rect x="8" y="8" width="12" height="12" rx="2" /><path d="M4 16V6a2 2 0 0 1 2-2h10" /></svg>
);
export const IconCheck = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={14} height={14} {...p}><path d="M20 6 9 17l-5-5" /></svg>
);
export const IconSpark = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={16} height={16} {...p}><path d="M12 3v5M12 16v5M3 12h5M16 12h5M6 6l3 3M18 18l-3-3M18 6l-3 3M6 18l3-3" /></svg>
);
export const IconLock = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={16} height={16} {...p}><rect x="4" y="10" width="16" height="10" rx="2" /><path d="M8 10V7a4 4 0 0 1 8 0v3" /></svg>
);
export const IconWarn = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={15} height={15} {...p}><path d="M12 3 22 20H2Z" /><path d="M12 9v5M12 17.5v.01" /></svg>
);
export const IconChain = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><rect x="3" y="9" width="8" height="8" rx="3" /><rect x="13" y="7" width="8" height="8" rx="3" /><path d="M9 11l6-2" /></svg>
);
export const IconRoots = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><circle cx="12" cy="6" r="3" /><path d="M12 9v4M12 13l-6 6M12 13l6 6M12 13l-2 7M12 13l2 7" /></svg>
);
export const IconSwap = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><path d="M4 8h13M17 8l-4-4M17 8l-4 4" /><path d="M20 16H7M7 16l4-4M7 16l4 4" /></svg>
);
export const IconCoin = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><circle cx="12" cy="12" r="9" /><path d="M12 7v10M9.5 9.5c0-1.4 1.2-2 2.5-2s2.5.7 2.5 1.8-1 1.5-2.5 1.9-2.5.8-2.5 1.9S10.7 15 12 15s2.5-.6 2.5-2" /></svg>
);
export const IconBridge = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><path d="M2 17c2-4 5-6 10-6s8 2 10 6" /><path d="M6 17v-4M12 17v-6M18 17v-4" /></svg>
);
export const IconSeal = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} {...p}><circle cx="12" cy="9" r="6" /><path d="M9 9l2 2 4-4" /><path d="M9 14.5 7 21l5-3 5 3-2-6.5" /></svg>
);
export const IconDownload = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={15} height={15} {...p}><path d="M12 3v12" /><path d="M7 10l5 5 5-5" /><path d="M4 19h16" /></svg>
);
export const IconChevron = (p: SVGProps<SVGSVGElement>) => (
  <svg {...base} width={11} height={11} {...p}><path d="M6 9l6 6 6-6" /></svg>
);
export const IconGithub = (p: SVGProps<SVGSVGElement>) => (
  <svg viewBox="0 0 24 24" width={17} height={17} fill="currentColor" {...p}>
    <path d="M12 .5C5.65.5.5 5.65.5 12c0 5.08 3.29 9.39 7.86 10.91.57.1.78-.25.78-.55 0-.27-.01-1.17-.02-2.12-3.2.7-3.88-1.36-3.88-1.36-.52-1.33-1.28-1.69-1.28-1.69-1.05-.72.08-.7.08-.7 1.16.08 1.77 1.19 1.77 1.19 1.03 1.76 2.7 1.25 3.36.96.1-.75.4-1.25.73-1.54-2.55-.29-5.24-1.28-5.24-5.7 0-1.26.45-2.28 1.19-3.09-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.18 1.18a11 11 0 0 1 5.79 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.83 1.19 3.09 0 4.43-2.7 5.41-5.27 5.69.42.36.78 1.07.78 2.15 0 1.56-.01 2.81-.01 3.19 0 .3.21.66.79.55A10.52 10.52 0 0 0 23.5 12C23.5 5.65 18.35.5 12 .5Z" />
  </svg>
);
