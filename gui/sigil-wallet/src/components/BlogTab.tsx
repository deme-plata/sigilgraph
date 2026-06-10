import { useEffect, useState } from 'react';
import { BookOpen, CalendarDays } from 'lucide-react';

interface BlogPost {
  slug: string;
  title: string;
  date: string;     // YYYY-MM-DD
  author: string;
  tags: string[];
  body: string[];   // paragraphs
}

/**
 * Blog — posts live in ./blog/posts.json (public dir), so new posts ship
 * without a rebuild: edit the JSON, redeploy the one file.
 */
export default function BlogTab() {
  const [posts, setPosts] = useState<BlogPost[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch('./blog/posts.json')
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data) => { if (!cancelled) setPosts(data.posts ?? []); })
      .catch((e) => { if (!cancelled) setError(String(e)); });
    return () => { cancelled = true; };
  }, []);

  return (
    <div className="px-4 pb-6 max-w-3xl">
      <div className="flex items-center gap-2 py-3 text-sm text-slate-300">
        <BookOpen className="w-4 h-4 text-quantum-cyan" />
        <span className="font-medium">SIGIL / Flux dev blog</span>
      </div>
      {error && (
        <div className="text-slate-500 text-sm py-6">No posts yet — the feed at ./blog/posts.json is unreachable ({error}).</div>
      )}
      {!posts && !error && (
        <div className="text-slate-500 text-sm py-6">loading posts…</div>
      )}
      {posts && posts.length === 0 && (
        <div className="text-slate-500 text-sm py-6">No posts published yet.</div>
      )}
      <div className="space-y-6">
        {posts?.map((p) => (
          <article key={p.slug} className="bg-slate-800/40 border border-slate-700/60 rounded-xl p-5">
            <h2 className="text-lg font-semibold text-slate-100">{p.title}</h2>
            <div className="flex items-center gap-3 mt-1 mb-3 text-xs text-slate-500">
              <span className="flex items-center gap-1"><CalendarDays className="w-3 h-3" />{p.date}</span>
              <span>{p.author}</span>
              <span className="flex gap-1">
                {p.tags.map((t) => (
                  <span key={t} className="px-1.5 py-0.5 rounded bg-slate-700/60 text-slate-400">{t}</span>
                ))}
              </span>
            </div>
            {p.body.map((para, i) => (
              <p key={i} className="text-sm text-slate-300 leading-relaxed mt-2">{para}</p>
            ))}
          </article>
        ))}
      </div>
    </div>
  );
}
