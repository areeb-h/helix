import { notFound } from "next/navigation";
import { Shell } from "@/components/Shell";
import { getDoc, listDocs } from "@/lib/content";
import { renderMarkdown } from "@/lib/markdown";

export function generateStaticParams() {
  return listDocs().map((d) => ({ slug: d.slug }));
}

export default async function DocPage({
  params,
}: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  const doc = getDoc(slug);
  if (!doc) notFound();

  const docs = listDocs();
  const nav = docs.map((d) => ({
    href: `/docs/${d.slug}`,
    label: d.title,
    active: d.slug === slug,
  }));

  return (
    <Shell nav={nav} navTitle="Documentation">
      <article className="max-w-3xl">
        <div className="mb-6 flex items-baseline justify-between gap-4 border-b border-zinc-900 pb-4">
          <h1 className="text-3xl font-bold tracking-tight">{doc.title}</h1>
          <code className="shrink-0 font-mono text-[11px] text-zinc-600">{doc.rel}</code>
        </div>
        <div
          className="prose-helix"
          dangerouslySetInnerHTML={{ __html: renderMarkdown(doc.markdown) }}
        />
      </article>
    </Shell>
  );
}
