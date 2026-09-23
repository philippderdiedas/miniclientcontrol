# Asset Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hovering an asset in the operator pages shows it, and every asset picker shows a thumbnail of its choice.

**Architecture:** One script, `web/preview.js`, renders an asset from its file in the page (image, first video frame, first PDF page via the vendored pdf.js) and offers `Preview.hover` and `Preview.follow`; three pages use it.

**Tech Stack:** vanilla JS; stdlib Python end-to-end test driving headless Chrome.

**Spec:** `docs/superpowers/specs/2026-09-23-asset-preview-design.md`

## Global Constraints

- `web/` is compiled in — `cargo build` before any Python test.
- Only `/uploads/<encodeURIComponent(local_path)>` from the controller; pdf.js and its worker from `/pdf.min.js` / `/pdf.worker.min.js`, never a CDN.
- `createElement`/`textContent` only, never `innerHTML` interpolation.
- Hover box at most 320×240; thumbnail 64×40.
- No `Co-Authored-By` / `Claude-Session` trailers.
- Stop any local instance before Python suites; `test_media.py` uses 3051 / 9252 / 9253.

---

### Task 1: `preview.js` and its test

**Files:** create `web/preview.js`; modify `tests/cast/test_media.py` (`browser_flow`, after `[112]`)

- [ ] **Step 1: The failing case** — at the end of `browser_flow`:

```python
    print("\n[113] hovering an asset shows it")
    assets_by_name = {a["filename"]: a for a in http("GET", "/api/assets", port=HTTP)[1]}
    admin_ws, _ = cdp.page_ws(ADMIN_CDP)
    async with cdp.Session(admin_ws) as admin:
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/assets.html"})
        await asyncio.sleep(1.5)

        async def hover_row(asset_id):
            return json.loads(await admin.eval(f"""(() => {{
                const row = [...document.querySelectorAll('#assetsBody tr')]
                  .find((tr) => tr.firstChild && tr.firstChild.textContent === '{asset_id}');
                const name = row && row.children[1];
                if (!name) return JSON.stringify({{ found: false }});
                name.dispatchEvent(new MouseEvent('mouseenter'));
                const box = document.querySelector('.asset-preview');
                const visual = box && box.firstElementChild;
                return JSON.stringify({{
                  found: true,
                  shown: !!box && box.style.display === 'block',
                  tag: visual ? visual.tagName.toLowerCase() : null,
                  src: visual ? visual.getAttribute('src') : null,
                }});
            }})()"""))

        image = await hover_row(assets_by_name["fit.png"]["id"])
        check("an image's name shows the image",
              image["shown"] and image["tag"] == "img"
              and image["src"].startswith("/uploads/") and image["src"].endswith("fit.png"), image)
        video = await hover_row(assets_by_name["recorded.webm"]["id"])
        check("a video's name shows its first frame", video["shown"] and video["tag"] == "video",
              video)

        await hover_row(assets_by_name["slides.pdf"]["id"])
        # The test PDF's first page carries a blue rectangle: finding its blue in
        # the rendered picture says pdf.js drew the page, not just an empty box.
        blue = None
        for _ in range(40):
            blue = json.loads(await admin.eval("""(() => {
                const img = document.querySelector('.asset-preview img');
                if (!img || img.dataset.pdf !== 'ready' || !img.naturalWidth)
                  return JSON.stringify({ ready: false, state: img && img.dataset.pdf });
                const c = document.createElement('canvas');
                c.width = img.naturalWidth; c.height = img.naturalHeight;
                const ctx = c.getContext('2d');
                ctx.drawImage(img, 0, 0);
                const data = ctx.getImageData(0, 0, c.width, c.height).data;
                let found = false;
                for (let i = 0; i < data.length; i += 4) {
                  if (data[i + 2] > 200 && data[i] < 80 && data[i + 1] < 80) { found = true; break; }
                }
                return JSON.stringify({ ready: true, found, width: c.width });
            })()"""))
            if blue.get("ready"):
                break
            await asyncio.sleep(0.25)
        check("a PDF's name shows its first page, drawn", blue.get("found") is True, blue)

        print("\n[113b] a playlist card's head and the pickers show it too")
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/playlist.html"})
        await asyncio.sleep(2)
        head = json.loads(await admin.eval(f"""(() => {{
            const src = [...document.querySelectorAll('.card .src')]
              .find((s) => s.textContent.startsWith('Asset #{assets_by_name["fit.png"]["id"]} '));
            if (!src) return JSON.stringify({{ found: false }});
            src.dispatchEvent(new MouseEvent('mouseenter'));
            const box = document.querySelector('.asset-preview');
            return JSON.stringify({{ found: true, shown: !!box && box.style.display === 'block',
              tag: box && box.firstElementChild ? box.firstElementChild.tagName.toLowerCase() : null }});
        }})()"""))
        check("an asset card's head shows the asset", head.get("shown") and head.get("tag") == "img",
              head)
        await admin.eval(f"""(() => {{
            const pick = document.getElementById('addAsset');
            pick.value = '{assets_by_name["slides.pdf"]["id"]}';
            pick.dispatchEvent(new Event('change', {{ bubbles: true }}));
        }})()""")
        thumb = None
        for _ in range(40):
            thumb = json.loads(await admin.eval("""(() => {
                const pick = document.getElementById('addAsset');
                const holder = pick.nextElementSibling;
                const img = holder && holder.classList.contains('asset-thumb') && holder.querySelector('img');
                return JSON.stringify({ holder: !!holder && holder.className,
                                        state: img ? img.dataset.pdf : null });
            })()"""))
            if thumb.get("state") == "ready":
                break
            await asyncio.sleep(0.25)
        check("the add form's picker shows the chosen PDF's first page beside it",
              thumb.get("state") == "ready", thumb)
```

- [ ] **Step 2: Run, see it fail** — `cargo build && cd tests/cast && python3 test_media.py | grep -E "FAIL|\[113"; cd ../..` → every `[113]` check fails (no `.asset-preview`).

- [ ] **Step 3: `web/preview.js`**

```js
// Asset previews for the operator pages: a box on hover, a thumbnail beside a
// picker. Rendered in the page from the asset's own file -- nothing is generated
// or stored on the device. createElement only; the path goes through
// encodeURIComponent, and only /uploads/ on this controller is ever loaded.
(() => {
  'use strict';

  const src = (asset) => '/uploads/' + encodeURIComponent(asset.local_path);

  function kindOf(asset) {
    const m = ((asset && asset.mimetype) || '').toLowerCase();
    if (!asset || !asset.local_path) return null;
    if (m.startsWith('image/')) return 'image';
    if (m.startsWith('video/')) return 'video';
    if (m === 'application/pdf') return 'pdf';
    return null;
  }

  // pdf.js is ~300 KB, so it is loaded on the first PDF preview only. Its worker
  // is served by the controller too: the device is often offline, and a CDN
  // worker is the mistake that once made every PDF fail on the display.
  let pdfjs = null;
  function loadPdfjs() {
    if (!pdfjs) {
      pdfjs = new Promise((resolve, reject) => {
        const ready = () => {
          globalThis.pdfjsLib.GlobalWorkerOptions.workerSrc = '/pdf.worker.min.js';
          resolve(globalThis.pdfjsLib);
        };
        if (globalThis.pdfjsLib) { ready(); return; }
        const script = document.createElement('script');
        script.src = '/pdf.min.js';
        script.onload = ready;
        script.onerror = reject;
        document.head.append(script);
      });
    }
    return pdfjs;
  }

  // A PDF's first page as a data URL, rendered once per asset per page load, so
  // the hover box and every thumbnail share one render.
  const firstPages = new Map();
  function firstPage(asset) {
    if (!firstPages.has(asset.id)) {
      firstPages.set(asset.id, loadPdfjs().then(async (lib) => {
        const pdf = await lib.getDocument(src(asset)).promise;
        const page = await pdf.getPage(1);
        const unscaled = page.getViewport({ scale: 1 });
        const viewport = page.getViewport({
          scale: 480 / Math.max(unscaled.width, unscaled.height),
        });
        const canvas = document.createElement('canvas');
        canvas.width = Math.ceil(viewport.width);
        canvas.height = Math.ceil(viewport.height);
        await page.render({ canvasContext: canvas.getContext('2d', { alpha: false }), viewport }).promise;
        return canvas.toDataURL('image/png');
      }).catch(() => null));
    }
    return firstPages.get(asset.id);
  }

  // The picture for one asset; the caller sizes it.
  function render(asset) {
    const kind = kindOf(asset);
    if (kind === 'image') {
      const img = document.createElement('img');
      img.loading = 'lazy';
      img.alt = '';
      img.src = src(asset);
      return img;
    }
    if (kind === 'video') {
      // `#t=0.1`: the frame at a tenth of a second, which is the first picture
      // rather than the black a video often starts on.
      const video = document.createElement('video');
      video.preload = 'metadata';
      video.muted = true;
      video.playsInline = true;
      video.src = src(asset) + '#t=0.1';
      return video;
    }
    if (kind === 'pdf') {
      const img = document.createElement('img');
      img.alt = '';
      img.dataset.pdf = 'loading';
      firstPage(asset).then((url) => {
        if (url) { img.src = url; img.dataset.pdf = 'ready'; }
        else img.dataset.pdf = 'failed';
      });
      return img;
    }
    return null;
  }

  const box = document.createElement('div');
  box.className = 'asset-preview';
  box.style.cssText = 'position:fixed; z-index:1000; pointer-events:none; display:none; '
    + 'background:#fff; border:1px solid #ccc; border-radius:4px; padding:4px; '
    + 'box-shadow:0 2px 8px rgba(0,0,0,.25);';
  let boxFor = null;

  function show(target, asset) {
    const visual = render(asset);
    if (!visual) return;
    visual.style.cssText = 'display:block; max-width:320px; max-height:240px;';
    box.replaceChildren(visual);
    if (!box.isConnected) document.body.append(box);
    const rect = target.getBoundingClientRect();
    // Below the element, or above it where the window ends.
    const below = rect.bottom + 6;
    box.style.left = Math.max(4, Math.min(rect.left, window.innerWidth - 340)) + 'px';
    box.style.top = (below + 250 > window.innerHeight ? Math.max(4, rect.top - 256) : below) + 'px';
    box.style.display = 'block';
    boxFor = target;
  }

  function hide(target) {
    if (boxFor !== target) return;
    box.style.display = 'none';
    box.replaceChildren();
    boxFor = null;
  }

  // Show `asset` while `element` is hovered or focused. tabindex, so a keyboard
  // and a tap on a touch screen get it too.
  function hover(element, asset) {
    if (!kindOf(asset)) return element;
    element.tabIndex = 0;
    element.style.cursor = 'help';
    element.addEventListener('mouseenter', () => show(element, asset));
    element.addEventListener('focus', () => show(element, asset));
    element.addEventListener('mouseleave', () => hide(element));
    element.addEventListener('blur', () => hide(element));
    return element;
  }

  // A thumbnail of whatever `select` has chosen. A native select shows nothing
  // while it is open, so this is the preview a picker can have. It renders only
  // once it is on screen, so a long playlist does not render a PDF per card.
  function follow(select, lookup) {
    const holder = document.createElement('span');
    holder.className = 'asset-thumb';
    holder.style.cssText = 'display:inline-block; width:64px; height:40px; '
      + 'vertical-align:middle; margin-left:.4rem; overflow:hidden;';
    let visible = false;
    let shown;
    const update = (force) => {
      if (!visible) return;
      const asset = lookup(select.value) || null;
      const id = asset ? asset.id : null;
      if (!force && id === shown) return;
      shown = id;
      const visual = asset ? render(asset) : null;
      if (visual) {
        visual.style.cssText = 'display:block; width:64px; height:40px; object-fit:contain;';
        holder.replaceChildren(visual);
        holder.title = asset.filename || '';
      } else {
        holder.replaceChildren();
        holder.title = '';
      }
    };
    select.addEventListener('change', () => update(false));
    new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        visible = true;
        update(false);
      }
    }).observe(holder);
    return { element: holder, refresh: () => update(true) };
  }

  globalThis.Preview = { hover, follow, kindOf };
})();
```

- [ ] **Step 4: Wire it up**

Each of `web/assets.html`, `web/playlist.html`, `web/admin.html`: add `<script src="/preview.js"></script>` directly before the page's inline `<script>`.

`web/assets.html`, in the row builder: `cell(tr, asset.id);` → `Preview.hover(cell(tr, asset.id), asset);` and `cell(tr, asset.filename);` → `Preview.hover(cell(tr, asset.filename), asset);`.

`web/playlist.html`:

- a lookup near `let assets = [];`:

```js
    const assetById = (value) => assets.find((a) => String(a.id) === String(value));
```

- in `buildCard`, the head's source span:

```js
      const srcLabel = el('span', { class: 'src', text: sourceLabel(item), title: sourceLabel(item) });
      if (item.asset_id !== null && item.asset_id !== undefined) {
        Preview.hover(srcLabel, {
          id: item.asset_id, local_path: item.local_path,
          mimetype: item.mimetype, filename: item.filename,
        });
      }
```

  and use `srcLabel` in place of the inline `el('span', { class: 'src', … })` in `head`.

- the card's picker: after `const assetPicker = isAsset ? assetSelect(item.asset_id) : null;`

```js
      const assetThumb = isAsset ? Preview.follow(assetPicker, assetById) : null;
```

  and `field('Asset', assetPicker)` → `field('Asset', el('span', {}, assetPicker, assetThumb.element))`.

- the add and override pickers: right after the `// --- add form` header:

```js
    const addThumb = Preview.follow(document.getElementById('addAsset'), assetById);
    document.getElementById('addAsset').after(addThumb.element);
    const ovThumb = Preview.follow(document.getElementById('ovAsset'), assetById);
    document.getElementById('ovAsset').after(ovThumb.element);
```

  and at the end of the asset-list reload (after the loop that rebuilds `#addAsset`/`#ovAsset`): `addThumb.refresh(); ovThumb.refresh();` — declared with `let` above that function if it runs before the add-form section; otherwise guard with `if (typeof addThumb !== 'undefined')`. (Check the order in the file: the reload function is defined earlier but *called* after the page has initialised, so the `const`s exist by then.)

- the item overlay's image picker in `overlayEditor`: after the `image` select is filled,

```js
      const imageThumb = Preview.follow(image, (v) => (assets || []).find((a) => String(a.id) === v));
```

  and `field('Bild', image)` → `field('Bild', el('span', {}, image, imageThumb.element))`.

`web/admin.html`: keep the assets `loadOverlayImages` fetched —

```js
    let overlayImages = [];
    const ovImageThumb = Preview.follow(
      document.getElementById('ovImage'),
      (v) => overlayImages.find((a) => String(a.id) === v));
    document.getElementById('ovImage').after(ovImageThumb.element);
```

  (placed before `loadOverlayImages`), in `loadOverlayImages` push every image asset into `overlayImages` and call `ovImageThumb.refresh()` at its end, and call `ovImageThumb.refresh()` right after the settings code sets `ovImage.value` (it fires no `change`).

- [ ] **Step 5: Run** — `cargo build && cd tests/cast && python3 test_media.py | grep -E "FAIL|ALL PASSED|FAILED"; cd ../..` → `ALL PASSED`.

- [ ] **Step 6: Commit** — `git add web/preview.js web/assets.html web/playlist.html web/admin.html tests/cast/test_media.py && git commit -m "Preview an asset on hover and beside every picker"`

### Task 2: Docs

- [ ] `docs/features.md` (*Assets*): "Hovering an asset's name — in the asset list, on a playlist card — shows it: the image, a video's first frame, a PDF's first page. Every asset picker shows a thumbnail of its choice. Rendered in the page from the file; nothing is stored."
- [ ] `docs/roadmap.md`: delete *Asset preview on hover*; in *A frozen screen is noticed* add: "Its screenshots are also the preview the admin status line should show — deliberately left out of the asset preview, because what a screen shows is not its asset's file."
- [ ] `tests/cast/README.md`: `test_media.py` line gains "asset previews".
- [ ] Spec → `**Status:** implemented`.
- [ ] Commit: `git add docs/ tests/cast/README.md && git commit -m "Document asset previews"`
