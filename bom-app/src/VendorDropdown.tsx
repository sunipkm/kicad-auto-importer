import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";

// Mirrors `kicad_auto_importer_core::parts_lookup::{VendorOffer, VendorCandidate, ScoredCandidate}`.
interface VendorOffer {
  seller: string;
  url: string;
  sku: string;
  price_summary: string;
  stock_status: "InStock" | "OutOfStock";
  stock_summary: string;
  stock_quantity: number;
  lifecycle_summary: string;
  lifecycle_concern: boolean;
  suggested_replacement: string;
  price_breaks: [number, number][];
}

export interface VendorCandidate {
  manufacturer: string;
  mpn: string;
  description: string;
  offer: VendorOffer;
}

export interface ScoredCandidate {
  candidate: VendorCandidate;
  purchase_qty: number;
  unit_price: number;
  total_price: number;
  feasible: boolean;
  score: number;
}

interface PartsCredentials {
  mouser_api_key: string;
  digikey_client_id: string;
  digikey_client_secret: string;
}

interface Props {
  mpn: string;
  neededQty: number;
  schPath: string;
  uuid: string;
  /// What the trigger button shows while collapsed — normally the
  /// row's last batch-run result summary; `null` before anything's
  /// been looked up yet.
  currentSummary: string | null;
  /// Highlights the trigger itself (not just the dropdown contents) —
  /// set when the row's own last result was an error or needed
  /// attention (not in stock / lifecycle concern), so a problem is
  /// visible without opening the dropdown at all.
  currentUnsafe: boolean;
  onApplied: (chosen: ScoredCandidate) => void;
}

/// Replaces the old "Choose Vendor…" modal: a dropdown, anchored under
/// its own trigger, ranking every raw candidate for `mpn` from every
/// configured vendor via `get_scored_candidates` (stock-gated-cheapest,
/// see `parts_lookup::score_candidates`) — cache-backed, so opening
/// this is instant for anything Populate/Generate BOM already looked up
/// recently. Picking a row here writes it onto the schematic instance
/// the same way an automatic batch run would
/// (`apply_vendor_choice`/`parts_lookup::apply_part_info`), just
/// manually overriding whichever candidate the automatic pass chose.
export function VendorDropdown({
  mpn,
  neededQty,
  schPath,
  uuid,
  currentSummary,
  currentUnsafe,
  onApplied,
}: Props) {
  const [open, setOpen] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [scored, setScored] = useState<ScoredCandidate[] | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [applying, setApplying] = useState(false);
  const [panelPos, setPanelPos] = useState<{ top: number; left: number } | null>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  // The panel is portaled to `document.body` (see the render below) so it
  // isn't clipped by `.table-scroll`'s `overflow: auto` — an
  // absolutely-positioned child of a table cell gets cut off at the
  // scroll container's edge otherwise. Position is computed from the
  // trigger's own on-screen rect each time it opens.
  const PANEL_WIDTH = 380;

  function positionPanel() {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const left = Math.min(rect.left, window.innerWidth - PANEL_WIDTH - 8);
    setPanelPos({ top: rect.bottom + 6, left: Math.max(8, left) });
  }

  useEffect(() => {
    if (!open) return;
    function onPointerDown(e: MouseEvent) {
      const target = e.target as Node;
      if (
        triggerRef.current &&
        !triggerRef.current.contains(target) &&
        panelRef.current &&
        !panelRef.current.contains(target)
      ) {
        setOpen(false);
      }
    }
    // Anchored positioning goes stale the moment the page scrolls or
    // resizes (the table body scrolling is the common case) — closing
    // rather than re-measuring on every scroll tick keeps this simple.
    function onScrollOrResize() {
      setOpen(false);
    }
    document.addEventListener("mousedown", onPointerDown);
    window.addEventListener("scroll", onScrollOrResize, true);
    window.addEventListener("resize", onScrollOrResize);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      window.removeEventListener("scroll", onScrollOrResize, true);
      window.removeEventListener("resize", onScrollOrResize);
    };
  }, [open]);

  async function fetchScored(forceRefresh: boolean) {
    setLoading(true);
    setError(null);
    try {
      const credentials = await invoke<PartsCredentials>("load_global_settings");
      const result = await invoke<ScoredCandidate[]>("get_scored_candidates", {
        mpn,
        neededQty,
        forceRefresh,
        credentials,
      });
      setScored(result);
    } catch (exc) {
      setError(String(exc));
    } finally {
      setLoading(false);
    }
  }

  function toggleOpen() {
    const next = !open;
    if (next) positionPanel();
    setOpen(next);
    setExpanded(null);
    if (next && scored === null) {
      fetchScored(false);
    }
  }

  async function choose(scoredCandidate: ScoredCandidate) {
    setApplying(true);
    setError(null);
    try {
      await invoke("apply_vendor_choice", {
        schPath,
        uuid,
        mpn,
        chosen: [scoredCandidate.candidate],
      });
      onApplied(scoredCandidate);
      setOpen(false);
    } catch (exc) {
      setError(String(exc));
    } finally {
      setApplying(false);
    }
  }

  const topUnsafe = !!scored && scored.length > 0 && !scored[0].feasible;

  const panel = open && panelPos && (
    <div
      className="vendor-dropdown-panel"
      ref={panelRef}
      style={{ position: "fixed", top: panelPos.top, left: panelPos.left }}
    >
      {loading && <p className="field-hint">Looking up candidates…</p>}
      {error && <p className="status-line status-error">{error}</p>}

      {!loading && scored && scored.length === 0 && (
        <p className="status-line status-warning">No candidates found for '{mpn}'.</p>
      )}

      {!loading && topUnsafe && (
        <p className="status-line status-error">
          ⚠ No candidate has enough stock to cover the needed quantity ({neededQty}) — review
          before ordering.
        </p>
      )}

      {!loading &&
        scored &&
        scored.map((s, i) => (
          <div
            key={`${s.candidate.offer.seller}-${s.candidate.offer.sku}-${i}`}
            className={`candidate-row ${!s.feasible ? "infeasible" : ""} ${i === 0 ? "best" : ""}`}
          >
            <button
              type="button"
              className="candidate-row-main"
              onClick={() => choose(s)}
              disabled={applying}
            >
              <span className="candidate-score" title="Score (cost, gated by available stock)">
                {s.score.toFixed(0)}
              </span>
              <span className="candidate-summary">
                <strong>{s.candidate.offer.seller}</strong> — {s.candidate.mpn} — $
                {s.total_price.toFixed(2)}
                {!s.feasible && " · not enough stock"}
                {s.candidate.offer.lifecycle_concern && " · ⚠ lifecycle"}
              </span>
            </button>
            <button
              type="button"
              className="btn-icon btn-icon-sm"
              onClick={() => setExpanded(expanded === i ? null : i)}
              aria-label="Details"
              title="Details"
            >
              {expanded === i ? "︿" : "﹀"}
            </button>

            {expanded === i && (
              <div className="candidate-detail">
                <p>{s.candidate.description}</p>
                <p className="field-hint">
                  SKU {s.candidate.offer.sku} · {s.candidate.offer.stock_summary} ·{" "}
                  {s.candidate.offer.lifecycle_summary}
                </p>
                <p className="field-hint">{s.candidate.offer.price_summary}</p>
                {s.candidate.offer.suggested_replacement && (
                  <p className="field-hint">
                    Suggested replacement: {s.candidate.offer.suggested_replacement}
                  </p>
                )}
              </div>
            )}
          </div>
        ))}

      <div className="form-actions">
        <button
          type="button"
          className="btn btn-sm"
          onClick={() => fetchScored(true)}
          disabled={loading || applying}
        >
          Refresh
        </button>
      </div>
    </div>
  );

  // Border color is the at-a-glance signal: green once a choice exists
  // and is safe (in stock, no lifecycle concern), red the moment it
  // isn't — so a row that needs a human's attention stands out without
  // opening the dropdown. Neutral (default border) only before
  // anything's been looked up at all.
  const triggerStatus = !currentSummary ? "" : currentUnsafe ? "unsafe" : "safe";

  return (
    <div className="vendor-dropdown">
      <button
        type="button"
        ref={triggerRef}
        className={`btn btn-sm vendor-dropdown-trigger ${triggerStatus}`}
        onClick={toggleOpen}
      >
        <span className="vendor-dropdown-trigger-text">{currentSummary ?? "Choose vendor…"}</span>
        <span className="vendor-dropdown-chevron">{open ? "▲" : "▼"}</span>
      </button>

      {panel && createPortal(panel, document.body)}
    </div>
  );
}
