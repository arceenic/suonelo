import { memo, useEffect, useRef, useState, type CSSProperties } from "react";

const MARQUEE_GAP_PX = 36;
const MARQUEE_SPEED_PX_PER_SEC = 38;
const MARQUEE_MIN_DURATION_SEC = 8;

export const MarqueeText = memo(function MarqueeText({ text, title }: {
  text: string;
  title?: string;
}) {
  const hostRef = useRef<HTMLElement>(null);
  const segmentRef = useRef<HTMLSpanElement>(null);
  const [state, setState] = useState({ overflow: false, duration: MARQUEE_MIN_DURATION_SEC });
  const overflowRef = useRef(false);
  overflowRef.current = state.overflow;

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const measure = () => {
      const segment = segmentRef.current;
      const available = host.clientWidth;
      if (available <= 0) return;
      const contentWidth = overflowRef.current && segment
        ? Math.ceil(segment.getBoundingClientRect().width)
        : host.scrollWidth;
      const overflow = contentWidth > available + 1;
      const duration = overflow
        ? Number(Math.max(MARQUEE_MIN_DURATION_SEC, (contentWidth + MARQUEE_GAP_PX) / MARQUEE_SPEED_PX_PER_SEC).toFixed(2))
        : MARQUEE_MIN_DURATION_SEC;
      setState(previous =>
        previous.overflow === overflow && previous.duration === duration
          ? previous
          : { overflow, duration },
      );
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(host);
    return () => observer.disconnect();
  }, [text]);

  const style = state.overflow
    ? ({ "--marquee-duration": `${state.duration}s`, "--marquee-gap": `${MARQUEE_GAP_PX}px` } as CSSProperties)
    : undefined;

  return (
    <small
      ref={hostRef}
      className={state.overflow ? "marquee-host" : undefined}
      data-overflow={state.overflow || undefined}
      style={style}
      title={title ?? text}
    >
      {state.overflow ? (
        <span className="marquee-track">
          <span ref={segmentRef} className="marquee-segment">{text}</span>
          <span className="marquee-segment" aria-hidden="true">{text}</span>
        </span>
      ) : text}
    </small>
  );
});
