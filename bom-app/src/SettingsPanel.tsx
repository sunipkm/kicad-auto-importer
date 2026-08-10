import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { BomConfigPanel } from "./BomConfigPanel";
import { SymbolColumnsPanel } from "./SymbolColumnsPanel";

// Mirrors `vendor_credentials::VendorCredentials` — bom-app's own
// settings.json, no longer shared with the egui desktop app — via the
// `load_vendor_credentials`/`save_vendor_credentials` Tauri commands
// (`src-tauri/src/lib.rs`).
interface VendorCredentials {
  mouser_api_key: string;
  digikey_client_id: string;
  digikey_client_secret: string;
  arrow_api_key: string;
}

type TestState = "idle" | "testing" | "ok" | { error: string };

function testStatusLabel(state: TestState): { text: string; className: string } | null {
  if (state === "idle") return null;
  if (state === "testing") return { text: "Testing…", className: "field-hint" };
  if (state === "ok") return { text: "✔ Connected", className: "status-line status-success" };
  return { text: `✘ ${state.error}`, className: "status-line status-error" };
}

/// A gear-icon button in the header that pops out the Mouser/DigiKey
/// credential fields on click, anchored to the top-right corner rather
/// than sitting inline in the page — an occasional, one-time setup
/// step shouldn't compete for space with the actual BOM tables.
export function SettingsPanel() {
  const [open, setOpen] = useState(false);
  const [settings, setSettings] = useState<VendorCredentials | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [mouserTest, setMouserTest] = useState<TestState>("idle");
  const [digikeyTest, setDigikeyTest] = useState<TestState>("idle");
  const [arrowTest, setArrowTest] = useState<TestState>("idle");
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open && !settings) {
      invoke<VendorCredentials>("load_vendor_credentials").then(setSettings);
    }
  }, [open, settings]);

  useEffect(() => {
    if (!open) return;
    function onPointerDown(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  async function save() {
    if (!settings) return;
    setStatus(null);
    setSaving(true);
    try {
      await invoke("save_vendor_credentials", { settings });
      setStatus("Saved.");
    } catch (exc) {
      setStatus(String(exc));
    } finally {
      setSaving(false);
    }
  }

  async function testMouser() {
    if (!settings) return;
    setMouserTest("testing");
    try {
      await invoke("test_mouser_credentials", { apiKey: settings.mouser_api_key });
      setMouserTest("ok");
    } catch (exc) {
      setMouserTest({ error: String(exc) });
    }
  }

  async function testDigikey() {
    if (!settings) return;
    setDigikeyTest("testing");
    try {
      await invoke("test_digikey_credentials", {
        clientId: settings.digikey_client_id,
        clientSecret: settings.digikey_client_secret,
      });
      setDigikeyTest("ok");
    } catch (exc) {
      setDigikeyTest({ error: String(exc) });
    }
  }

  async function testArrow() {
    if (!settings) return;
    setArrowTest("testing");
    try {
      await invoke("test_arrow_credentials", { apiKey: settings.arrow_api_key });
      setArrowTest("ok");
    } catch (exc) {
      setArrowTest({ error: String(exc) });
    }
  }

  const mouserStatus = testStatusLabel(mouserTest);
  const digikeyStatus = testStatusLabel(digikeyTest);
  const arrowStatus = testStatusLabel(arrowTest);

  return (
    <div className="settings-popover" ref={containerRef}>
      <button
        type="button"
        className="btn btn-icon"
        title="API Settings"
        aria-label="API Settings"
        onClick={() => setOpen((v) => !v)}
      >
        ⚙
      </button>

      {open && (
        <div className="settings-popover-panel">
          <h3>API Settings</h3>
          <p className="field-hint">
            Mouser/DigiKey API credentials for this app's part lookups.
          </p>

          {!settings ? (
            <p className="field-hint">Loading…</p>
          ) : (
            <>
              <label className="settings-field">
                Mouser API Key
                <input
                  type="password"
                  value={settings.mouser_api_key}
                  onChange={(e) => {
                    setSettings({ ...settings, mouser_api_key: e.currentTarget.value });
                    setMouserTest("idle");
                  }}
                />
              </label>
              <div className="settings-field-test">
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={testMouser}
                  disabled={mouserTest === "testing" || !settings.mouser_api_key.trim()}
                >
                  Test
                </button>
                {mouserStatus && (
                  <span className={mouserStatus.className}>{mouserStatus.text}</span>
                )}
              </div>

              <label className="settings-field">
                DigiKey Client ID
                <input
                  type="text"
                  value={settings.digikey_client_id}
                  onChange={(e) => {
                    setSettings({ ...settings, digikey_client_id: e.currentTarget.value });
                    setDigikeyTest("idle");
                  }}
                />
              </label>
              <label className="settings-field">
                DigiKey Client Secret
                <input
                  type="password"
                  value={settings.digikey_client_secret}
                  onChange={(e) => {
                    setSettings({
                      ...settings,
                      digikey_client_secret: e.currentTarget.value,
                    });
                    setDigikeyTest("idle");
                  }}
                />
              </label>
              <div className="settings-field-test">
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={testDigikey}
                  disabled={
                    digikeyTest === "testing" ||
                    !settings.digikey_client_id.trim() ||
                    !settings.digikey_client_secret.trim()
                  }
                >
                  Test
                </button>
                {digikeyStatus && (
                  <span className={digikeyStatus.className}>{digikeyStatus.text}</span>
                )}
              </div>

              <label className="settings-field">
                Arrow API Key
                <input
                  type="password"
                  value={settings.arrow_api_key}
                  onChange={(e) => {
                    setSettings({ ...settings, arrow_api_key: e.currentTarget.value });
                    setArrowTest("idle");
                  }}
                />
              </label>
              <div className="settings-field-test">
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={testArrow}
                  disabled={arrowTest === "testing" || !settings.arrow_api_key.trim()}
                >
                  Test
                </button>
                {arrowStatus && (
                  <span className={arrowStatus.className}>{arrowStatus.text}</span>
                )}
              </div>

              <div className="form-actions">
                <button type="button" className="btn btn-primary" onClick={save} disabled={saving}>
                  {saving ? "Saving…" : "Save"}
                </button>
              </div>
              {status && <p className="field-hint">{status}</p>}

              <details className="settings-section">
                <summary className="settings-section-title">BOM Settings…</summary>
                <BomConfigPanel />
              </details>

              <details className="settings-section">
                <summary className="settings-section-title">Symbol Table Columns…</summary>
                <SymbolColumnsPanel />
              </details>
            </>
          )}
        </div>
      )}
    </div>
  );
}
