import { Popover } from "@base-ui/react/popover";
import { CircleGauge, Minus } from "lucide-react";
import { memo, useId, useRef, useState, useSyncExternalStore } from "react";
import type { ConnectionLimit, QueueSnapshot } from "@/lib/desktop";
import { Button } from "./ui/primitives";

const limits = ["auto", 1, 2, 4, 8, 16, 32, "unlimited"] as const satisfies readonly ConnectionLimit[];
const limitLabel = (limit: ConnectionLimit) => limit === "auto" ? "Auto" : limit === "unlimited" ? "Unlimited" : String(limit);

function SpeedTrigger({
  triggerRef, disabled, subscribeSpeed, getSpeed,
}: {
  triggerRef: React.RefObject<HTMLButtonElement | null>;
  disabled?: boolean;
  subscribeSpeed: (listener: () => void) => () => void;
  getSpeed: () => number;
}) {
  const bytesPerSecond = useSyncExternalStore(subscribeSpeed, getSpeed, getSpeed);
  const formatted = (bytesPerSecond / 1_000_000).toFixed(2);
  return (
    <Popover.Trigger id="transfer-speed" ref={triggerRef} className="speed-trigger" disabled={disabled}
      aria-label={`Aggregate network download speed, ${formatted} MB/s. Max connections`}>
      <CircleGauge className="icon" aria-hidden /> {formatted} MB/s
    </Popover.Trigger>
  );
}

export function ConnectionSlider({ label, value, change, disabled }: {
  label: string; value: ConnectionLimit; change: (value: ConnectionLimit) => void; disabled?: boolean;
}) {
  const id = useId();
  const index = limits.indexOf(value);
  return <div className="field connection-slider">
    <div className="limit-readout"><label htmlFor={id}>{label}</label><output htmlFor={id}>{limitLabel(value)}</output></div>
    <div className="limit-scroll">
      <div className="limit-scale">
        <input id={id} type="range" min={0} max={limits.length - 1} step={1} value={index}
          aria-valuetext={limitLabel(value)} disabled={disabled}
          onChange={event => change(limits[event.currentTarget.valueAsNumber])} />
        <div className="limit-steps" aria-hidden>
          {limits.map(limit => <span key={limit} data-selected={limit === value}>{limitLabel(limit)}</span>)}
        </div>
      </div>
    </div>
  </div>;
}

export function SpeedControl({ queue, saved, save, disabled, subscribeSpeed, getSpeed }: {
  queue: QueueSnapshot;
  saved: ConnectionLimit;
  save: (value: ConnectionLimit) => Promise<void>;
  disabled?: boolean;
  subscribeSpeed: (listener: () => void) => () => void;
  getSpeed: () => number;
}) {
  const host = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<ConnectionLimit | undefined>();
  const [saving, setSaving] = useState(false);
  const mixed = queue.liveLimit === null;
  const value = draft ?? queue.liveLimit ?? saved;

  async function commit() {
    if (saving) return;
    setSaving(true);
    try {
      await save(value);
      setDraft(undefined);
    } catch {
      // The app reports the error. Keep the draft available for retry.
    } finally {
      setSaving(false);
    }
  }

  return <div className="speed-host" ref={host}>
    <Popover.Root open={open} onOpenChange={next => {
      setOpen(next);
      if (!next) setDraft(undefined);
    }}>
      <Popover.Trigger render={props => (
        <SpeedTrigger {...props} triggerRef={trigger} disabled={disabled} subscribeSpeed={subscribeSpeed} getSpeed={getSpeed} />
      )} />
      <Popover.Portal container={host}>
        <Popover.Positioner className="dropdown-position" side="top" align="end" sideOffset={8} collisionPadding={8}>
          <Popover.Popup className="connections-popup" finalFocus={trigger}>
            <div className="connections-head">
              <Popover.Title>Max connections</Popover.Title>
              <Popover.Close render={<Button className="ghost" aria-label="Close max connections"><Minus className="icon" aria-hidden /></Button>} />
            </div>
            {mixed && <p className="dim">Mixed limits across queued jobs.</p>}
            {queue.requestedMode !== queue.effectiveMode && <p className="dim" role="status">Download mode change pending.</p>}
            <ConnectionSlider label={queue.effectiveMode === "concurrent" ? "Live limit per job" : "Live limit"}
              value={value} change={setDraft} disabled={saving} />
            <Button className="primary" disabled={saving || (!mixed && value === queue.liveLimit)} onClick={() => void commit()}>
              {saving ? "Saving…" : "Save"}
            </Button>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  </div>;
}
