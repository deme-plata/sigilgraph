import { useState, lazy, Suspense } from 'react';
import ExplorerScreen from './ExplorerScreen';

const CodeActivityTab = lazy(() => import('./CodeActivityTab'));
const BlogTab = lazy(() => import('./BlogTab'));

type HubTab = 'activity' | 'code' | 'blog';

interface ExplorerHubProps {
  isActive: boolean;
}

/**
 * Explorer hub — wraps the chain Activity explorer with two side missions:
 * Code Activity (the flux-rev provenance feed) and the Blog.
 * The chain ExplorerScreen stays mounted at all times (see App.tsx note:
 * on remount its 7 optional-metric requests contend with core stats for
 * rate-limiter slots and stats never load).
 */
export default function ExplorerHub({ isActive }: ExplorerHubProps) {
  const [tab, setTab] = useState<HubTab>('activity');

  const tabs: { id: HubTab; label: string }[] = [
    { id: 'activity', label: 'Activity' },
    { id: 'code', label: 'Code Activity' },
    { id: 'blog', label: 'Blog' },
  ];

  return (
    <div className="flex flex-col h-full">
      <div className="flex gap-1 px-4 pt-3 pb-1">
        {tabs.map((t) => (
          <button
            key={t.id}
            onClick={() => setTab(t.id)}
            className={`px-4 py-1.5 rounded-t-lg text-sm font-medium transition-colors ${
              tab === t.id
                ? 'bg-slate-800/80 text-quantum-cyan border-b-2 border-quantum-cyan'
                : 'text-slate-400 hover:text-slate-200 hover:bg-slate-800/40'
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>
      {/* Chain activity stays mounted; only its visibility + isActive flag toggle */}
      <div style={{ display: tab === 'activity' ? 'block' : 'none' }}>
        <ExplorerScreen isActive={isActive && tab === 'activity'} />
      </div>
      {tab === 'code' && (
        <Suspense fallback={null}>
          <CodeActivityTab />
        </Suspense>
      )}
      {tab === 'blog' && (
        <Suspense fallback={null}>
          <BlogTab />
        </Suspense>
      )}
    </div>
  );
}
