import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface BomConfig {
  passive_extra_minimum: number;
}

export function BomConfigPanel() {
  const [config, setConfig] = useState<BomConfig | null>(null);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    invoke<BomConfig>("load_bom_config").then(setConfig);
  }, []);

  async function save() {
    if (!config) return;
    setSaving(true);
    setStatus(null);
    try {
      await invoke("save_bom_config", { config });
      setStatus("Saved.");
    } catch (exc) {
      setStatus(String(exc));
    } finally {
      setSaving(false);
    }
  }

  if (!config) return <p className="field-hint">Loading…</p>;

  return (
    <>
      <label className="settings-field">
        Passive Extra Minimum
        <input
          type="number"
          min="0"
          value={config.passive_extra_minimum}
          onChange={(e) =>
            setConfig({ ...config, passive_extra_minimum: Number(e.currentTarget.value) })
          }
        />
        <span className="field-hint">
          Minimum spare pieces to add to passive components. Set to 0 for no minimum.
        </span>
      </label>

      <div className="form-actions">
        <button type="button" className="btn btn-primary btn-sm" onClick={save} disabled={saving}>
          {saving ? "Saving…" : "Save"}
        </button>
        {status && <span className="field-hint">{status}</span>}
      </div>
    </>
  );
}
