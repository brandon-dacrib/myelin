import { Fragment, type ReactNode } from "react";

/** `send \`login qr\`` → the command in an inline code span, the rest as text. */
export function withInlineCode(text: string): ReactNode {
  const parts = text.split("`");
  return parts.map((part, i) =>
    i % 2 === 1 ? (
      <code key={i} className="rounded-xs bg-surface-sunken px-1 py-0.5 font-identifier text-text">
        {part}
      </code>
    ) : (
      <Fragment key={i}>{part}</Fragment>
    ),
  );
}
