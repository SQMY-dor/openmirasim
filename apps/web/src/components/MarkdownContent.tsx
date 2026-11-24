import { Check, Copy } from "lucide-react";
import { Fragment, memo, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

interface MarkdownContentProps {
  text: string;
  streaming?: boolean;
}

type Block =
  | { type: "code"; lang: string; content: string }
  | { type: "text"; content: string };

function splitFencedBlocks(raw: string): Block[] {
  const normalized = raw.replace(/\r\n?/g, "\n");
  const blocks: Block[] = [];
  const regex = /```([^\s`]*)\n([\s\S]*?)```/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = regex.exec(normalized)) !== null) {
    if (match.index > lastIndex) {
      blocks.push({ type: "text", content: normalized.slice(lastIndex, match.index) });
    }
    blocks.push({
      type: "code",
      lang: match[1] || "text",
      content: match[2].replace(/\n$/, ""),
    });
    lastIndex = match.index + match[0].length;
  }

  if (lastIndex < normalized.length) {
    blocks.push({ type: "text", content: normalized.slice(lastIndex) });
  }

  return blocks;
}

function renderInline(line: string, keyPrefix: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const tokenRegex = /(`[^`]+`|\*\*[^*]+\*\*)/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  let partIdx = 0;

  while ((match = tokenRegex.exec(line)) !== null) {
    if (match.index > lastIndex) {
      nodes.push(line.slice(lastIndex, match.index));
    }
    const token = match[0];
    if (token.startsWith("`") && token.endsWith("`")) {
      nodes.push(
        <code key={`${keyPrefix}-c-${partIdx}`} className="inline-code">
          {token.slice(1, -1)}
        </code>
      );
    } else if (token.startsWith("**") && token.endsWith("**")) {
      nodes.push(
        <strong key={`${keyPrefix}-b-${partIdx}`}>{token.slice(2, -2)}</strong>
      );
    }
    lastIndex = match.index + token.length;
    partIdx += 1;
  }

  if (lastIndex < line.length) {
    nodes.push(line.slice(lastIndex));
  }

  return nodes;
}

function CodeBlock({ lang, content }: { lang: string; content: string }) {
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
      }
    };
  }, []);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(content);
      setCopied(true);
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
      }
      timerRef.current = window.setTimeout(() => {
        setCopied(false);
        timerRef.current = null;
      }, 1500);
    } catch {
      // clipboard unavailable in restricted contexts
    }
  };

  return (
    <div className="code-block">
      <div className="code-block-header">
        <span>{lang}</span>
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          onClick={() => void handleCopy()}
          aria-label="复制代码"
        >
          {copied ? <Check size={13} /> : <Copy size={13} />}
          <span>{copied ? "已复制" : "复制"}</span>
        </button>
      </div>
      <pre>
        <code>{content}</code>
      </pre>
    </div>
  );
}

function renderTextBlock(content: string, blockIdx: number): ReactNode[] {
  const chunks = content.trim().split(/\n\n+/);
  const elements: ReactNode[] = [];

  chunks.forEach((chunk, chunkIdx) => {
    const trimmed = chunk.trim();
    if (!trimmed) return;
    const lines = trimmed.split("\n");
    const isUl = lines.every((l) => /^[-*]\s+/.test(l.trim()));
    const isOl = lines.every((l) => /^\d+\.\s+/.test(l.trim()));

    if (isUl) {
      elements.push(
        <ul key={`ul-${blockIdx}-${chunkIdx}`}>
          {lines.map((l, li) => (
            <li key={`li-${blockIdx}-${chunkIdx}-${li}`}>
              {renderInline(l.trim().replace(/^[-*]\s+/, ""), `u-${blockIdx}-${chunkIdx}-${li}`)}
            </li>
          ))}
        </ul>
      );
      return;
    }

    if (isOl) {
      elements.push(
        <ol key={`ol-${blockIdx}-${chunkIdx}`}>
          {lines.map((l, li) => (
            <li key={`oli-${blockIdx}-${chunkIdx}-${li}`}>
              {renderInline(l.trim().replace(/^\d+\.\s+/, ""), `o-${blockIdx}-${chunkIdx}-${li}`)}
            </li>
          ))}
        </ol>
      );
      return;
    }

    elements.push(
      <p key={`p-${blockIdx}-${chunkIdx}`}>
        {lines.map((l, li) => (
          <Fragment key={`l-${blockIdx}-${chunkIdx}-${li}`}>
            {li > 0 && <br />}
            {renderInline(l, `p-${blockIdx}-${chunkIdx}-${li}`)}
          </Fragment>
        ))}
      </p>
    );
  });

  return elements;
}

export const MarkdownContent = memo(function MarkdownContent({
  text,
  streaming,
}: MarkdownContentProps) {
  const blocks = useMemo(() => splitFencedBlocks(text), [text]);

  return (
    <div className="md-content">
      {blocks.map((block, idx) =>
        block.type === "code" ? (
          <CodeBlock key={`code-${idx}`} lang={block.lang} content={block.content} />
        ) : (
          <Fragment key={`text-${idx}`}>{renderTextBlock(block.content, idx)}</Fragment>
        )
      )}
      {streaming && <span className="streaming-cursor" aria-hidden="true" />}
    </div>
  );
});
