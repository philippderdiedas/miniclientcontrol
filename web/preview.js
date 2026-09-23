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
