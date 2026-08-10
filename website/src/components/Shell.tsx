import Link from "next/link";

export function Shell({
  children,
  nav,
  navTitle,
}: {
  children: React.ReactNode;
  nav?: { href: string; label: string; active?: boolean }[];
  navTitle?: string;
}) {
  return (
    <div className="min-h-screen bg-zinc-950 font-sans text-zinc-100">
      <header className="sticky top-0 z-10 border-b border-zinc-900 bg-zinc-950/85 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
          <Link href="/" className="flex items-baseline gap-2">
            <span className="text-lg font-bold tracking-tight text-emerald-400">helix</span>
            <span className="hidden text-xs text-zinc-500 sm:inline">
              three engines, one answer
            </span>
          </Link>
          <nav className="flex gap-6 text-sm text-zinc-400">
            <Link href="/docs" className="hover:text-zinc-100">Docs</Link>
            <Link href="/tour" className="hover:text-zinc-100">Tour</Link>
            <Link href="/bench" className="hover:text-zinc-100">Benchmarks</Link>
          </nav>
        </div>
      </header>

      <div className="mx-auto flex max-w-6xl gap-10 px-6 py-10">
        {nav && nav.length > 0 ? (
          <aside className="hidden w-56 shrink-0 lg:block">
            <div className="sticky top-24">
              {navTitle ? (
                <div className="mb-3 text-[11px] font-semibold uppercase tracking-widest text-zinc-600">
                  {navTitle}
                </div>
              ) : null}
              <ul className="space-y-0.5 text-sm">
                {nav.map((n) => (
                  <li key={n.href}>
                    <Link
                      href={n.href}
                      className={`block rounded-md px-2.5 py-1.5 ${
                        n.active
                          ? "bg-emerald-950/60 text-emerald-300"
                          : "text-zinc-400 hover:bg-zinc-900 hover:text-zinc-200"
                      }`}
                    >
                      {n.label}
                    </Link>
                  </li>
                ))}
              </ul>
            </div>
          </aside>
        ) : null}
        <main className="min-w-0 flex-1">{children}</main>
      </div>

      <footer className="border-t border-zinc-900 py-8 text-center text-xs text-zinc-600">
        Every example on this site is executed by the repository&apos;s test gate on all
        three engines.
      </footer>
    </div>
  );
}
