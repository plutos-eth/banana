/**
 * Phase 0 placeholder. The six views of spec §8 are built in phase 5.
 *
 * Note what is already true here and must stay true: every value below comes from
 * tokens.css by class, not by inline style. `scripts/check-ui-invariants.ps1` fails the
 * build if a colour or a pixel value appears in this file.
 */
export function App() {
  return (
    <main className="shell">
      <h1 className="shell__title">quarrel</h1>
      <p className="shell__note">phase 0 skeleton</p>
    </main>
  );
}
