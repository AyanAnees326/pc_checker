import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Inventory } from "./types";

export default function App() {
  const [inventory, setInventory] = useState<Inventory | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function runScan() {
    setScanning(true);
    setError(null);
    try {
      setInventory(await invoke<Inventory>("collect_inventory"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  }

  return (
    <main className="app">
      <header>
        <h1>PC Checker</h1>
        <p className="tagline">Inspect a used machine before you pay for it.</p>
      </header>

      <button onClick={runScan} disabled={scanning}>
        {scanning ? "Scanning…" : "Run Quick Scan"}
      </button>

      {error && <p className="error">{error}</p>}

      {inventory && (
        <pre className="raw">{JSON.stringify(inventory, null, 2)}</pre>
      )}
    </main>
  );
}
