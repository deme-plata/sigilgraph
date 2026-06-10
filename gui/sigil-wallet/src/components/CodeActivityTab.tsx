import { useState } from 'react';
import { GitBranch, ExternalLink } from 'lucide-react';

/**
 * Code Activity — embeds the flux-rev vite app ("code, reviewed by machines"):
 * commit provenance across the Quillon Graph / SIGIL / flux repos.
 * The bundle is shipped same-origin under ./flux-rev/ (copied from
 * /home/storage/deepseek-codewhale/flux-rev/dist at build time).
 */
export default function CodeActivityTab() {
  const [loaded, setLoaded] = useState(false);

  return (
    <div className="flex flex-col h-full px-4 pb-4">
      <div className="flex items-center justify-between py-2">
        <div className="flex items-center gap-2 text-sm text-slate-300">
          <GitBranch className="w-4 h-4 text-quantum-cyan" />
          <span className="font-medium">Flux Rev</span>
          <span className="text-slate-500">— code, reviewed by machines</span>
        </div>
        <a
          href="./flux-rev/index.html"
          target="_blank"
          rel="noreferrer"
          className="flex items-center gap-1 text-xs text-slate-400 hover:text-quantum-cyan transition-colors"
        >
          open full page <ExternalLink className="w-3 h-3" />
        </a>
      </div>
      {!loaded && (
        <div className="text-slate-500 text-sm py-8 text-center">loading code activity…</div>
      )}
      <iframe
        src="./flux-rev/index.html"
        title="Flux Rev — code activity"
        onLoad={() => setLoaded(true)}
        className="w-full flex-1 rounded-xl border border-slate-700/60 bg-slate-900"
        style={{ minHeight: '70vh', display: loaded ? 'block' : 'none' }}
      />
    </div>
  );
}
