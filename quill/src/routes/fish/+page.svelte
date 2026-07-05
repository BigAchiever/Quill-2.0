<script lang="ts">
  // The free-floating desktop fish (its own borderless, non-activating window). DRAG it to
  // move it around the desktop; a plain CLICK (no drag) sends it back to the dock.
  //
  // The drag is handled natively in Rust (it follows the global cursor and repositions the
  // window) instead of the native window-drag — that would activate our app and steal the
  // user's text-field focus. Here we just signal press/release.
  import { invoke } from "@tauri-apps/api/core";

  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    const target = e.currentTarget as HTMLElement;
    try {
      target.setPointerCapture(e.pointerId);
    } catch {
      /* ignore */
    }
    void invoke("fish_drag_start");
    const up = () => {
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      void invoke("fish_drag_stop"); // Rust decides: moved → stay, not moved → recall
    };
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }
</script>

<button class="fish" onpointerdown={onPointerDown} aria-label="drag to move, click to send to the dock">
  <svg viewBox="40 25 110 70">
    <ellipse cx="100" cy="60" rx="40" ry="30" fill="#ff7a2e" />
    <path d="M72 60 C 46 38, 30 44, 28 60 C 30 76, 46 82, 72 60 Z" fill="#ff5814" />
    <circle cx="124" cy="53" r="7" fill="#fff" />
    <circle cx="126" cy="53" r="3.4" fill="#1b1b1b" />
  </svg>
</button>

<style>
  :global(html), :global(body) {
    margin: 0; padding: 0; background: transparent !important; overflow: hidden;
  }
  .fish {
    width: 100vw; height: 100vh; border: 0; background: transparent; padding: 0;
    cursor: grab; display: grid; place-items: center;
  }
  .fish:active { cursor: grabbing; }
  .fish svg { width: 86%; height: 86%; filter: drop-shadow(0 6px 14px rgba(0, 0, 0, 0.45)); }
</style>
