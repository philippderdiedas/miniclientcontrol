(() => {
  // Counting passes for an item that moves on after N of them. One factory for
  // all three runtimes (this one, the PDF viewer, the media viewer), so they
  // count the same way; browser.rs reads `globalThis.__advance.state()`.
  if (!globalThis.__advanceCounter) {
    globalThis.__advanceCounter = (source) => ({
      source,
      target: 0,
      passes: 0,
      progressTs: performance.now(),
      passStartTs: performance.now(),
      state() {
        return { passes: this.passes, idle_ms: Math.round(performance.now() - this.progressTs), source: this.source };
      },
      // Called by the controller when the item starts: counting begins now,
      // and with a target the content stops at the end of the last pass
      // instead of starting over -- the controller polls, and would otherwise
      // catch the top of the page flashing past.
      reset(count) {
        this.target = Number(count) > 0 ? Number(count) : 0;
        this.passes = 0;
        this.progressTs = this.passStartTs = performance.now();
      },
      done() {
        return this.target > 0 && this.passes >= this.target;
      },
      // The content is moving, or holding on purpose. A deliberate pause is
      // progress; only a stuck one is not.
      alive(ts) {
        this.progressTs = ts;
      },
      // At the end of the content. True: stay where you are (the pass is not
      // long enough yet, or the last one is complete). False: this pass is
      // counted, go back to the start for the next.
      atEnd(ts, minMs) {
        this.progressTs = ts;
        if (this.done()) return true;
        if (ts - this.passStartTs < minMs) return true;
        this.passes += 1;
        if (this.done()) return true;
        this.passStartTs = ts;
        return false;
      },
    });
  }
  const MIN_PASS_MS = 3000;
  // Installed only when the page has none: a viewer that counts for itself (a
  // video, a paged PDF) installs its own after this first run, and the
  // controller's re-evaluation after navigation must not take it back.
  if (!globalThis.__advance) globalThis.__advance = globalThis.__advanceCounter('scroll');

  if (!globalThis.__asLog) {
    globalThis.__asLog = (...args) => {
      try {
        const href = (location && location.href) ? location.href : '(no-location)';
        if (!globalThis.__asBuffer) globalThis.__asBuffer = [];
        const normalize = (v) => {
          if (v == null) return String(v);
          if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return String(v);
          try { return JSON.stringify(v); } catch (_) { return '[object]'; }
        };
        globalThis.__asBuffer.push([Date.now(), href, ...args.map(normalize)]);
        if (globalThis.__asBuffer.length > 250) globalThis.__asBuffer.shift();
        console.log('[autoscroll]', href, ...args);
      } catch (_) {}
    };
  }

  if (!globalThis.__asNetwork) {
    globalThis.__asNetwork = {
      pending: 0,
      lastActivityTs: Date.now(),
      bump() {
        this.lastActivityTs = Date.now();
      },
      inc() {
        this.pending += 1;
        this.bump();
      },
      dec() {
        this.pending = Math.max(0, this.pending - 1);
        this.bump();
      },
      idleForMs() {
        return Math.max(0, Date.now() - this.lastActivityTs);
      }
    };
  }

  if (!globalThis.__asNetworkPatched) {
    globalThis.__asNetworkPatched = true;
    try {
      const net = globalThis.__asNetwork;

      if (typeof window.fetch === 'function') {
        const originalFetch = window.fetch.bind(window);
        window.fetch = (...args) => {
          net.inc();
          return originalFetch(...args).finally(() => net.dec());
        };
      }

      const xhrOpen = XMLHttpRequest.prototype.open;
      const xhrSend = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.open = function (...args) {
        this.__asTracked = true;
        return xhrOpen.apply(this, args);
      };
      XMLHttpRequest.prototype.send = function (...args) {
        if (this.__asTracked) {
          net.inc();
          this.addEventListener('loadend', () => net.dec(), { once: true });
        }
        return xhrSend.apply(this, args);
      };

      if ('PerformanceObserver' in window) {
        const po = new PerformanceObserver(() => net.bump());
        po.observe({ type: 'resource', buffered: true });
      }
    } catch (_) {}
  }

  if (globalThis.__as && globalThis.__as.__version === 3) {
    return;
  }

  const api = {
    __version: 3,
    enabled: false,
    mode: 'auto',
    pxPerSec: 120,
    stepPx: 900,
    stepTimeMs: 0,
    stepDelayMs: 2000,
    topDelayMs: 0,
    returnDelayMs: 0,
    raf: 0,
    lastTs: 0,
    holdUntilTs: 0,
    nextStepTs: 0,
    scrollEl: null,
    repickCounter: 0,

    // The page's pass counter while it is ours: a viewer that counts for
    // itself (a video, a paged PDF) owns `__advance` instead.
    adv() {
      const a = globalThis.__advance;
      return a && a.source === 'scroll' ? a : null;
    },

    // Nothing to scroll at all -- the page fits the screen. Not the same as a
    // taller page that will not move: that one reports no progress and the
    // controller's stall timeout moves it on.
    nothingToScroll() {
      const wm = this.windowMetrics();
      if (Math.max(0, wm.height - wm.viewport) > 1) return false;
      const el = this.scrollEl;
      if (!el || this.isRoot(el)) return true;
      const em = this.elementMetrics(el);
      return Math.max(0, em.height - em.viewport) <= 1;
    },

    isRoot(el) {
      return !el || el === window || el === document || el === document.documentElement || el === document.body || el === document.scrollingElement;
    },

    describeElement(el) {
      if (this.isRoot(el)) return 'root';
      try {
        const tag = (el.tagName || 'node').toLowerCase();
        const id = el.id ? `#${el.id}` : '';
        const cls = (typeof el.className === 'string' && el.className.trim())
          ? `.${el.className.trim().replace(/\s+/g, '.')}`
          : '';
        return `${tag}${id}${cls}`;
      } catch (_) {
        return 'node';
      }
    },

    pickScrollableElement() {
      const root = document.scrollingElement || document.documentElement || document.body;
      const candidates = [];
      const seen = new Set();

      const canScrollByWrite = (el) => {
        if (!el || this.isRoot(el) || typeof el.scrollTop !== 'number') return false;
        try {
          const before = el.scrollTop;
          el.scrollTop = before + 1;
          const after = el.scrollTop;
          el.scrollTop = before;
          return Math.abs(after - before) > 0;
        } catch (_) {
          return false;
        }
      };

      const centerPointChainCandidate = () => {
        try {
          const x = Math.max(1, Math.floor((window.innerWidth || 1280) * 0.5));
          const y = Math.max(1, Math.floor((window.innerHeight || 720) * 0.5));

          const chain = [];
          let curRoot = document;
          for (let i = 0; i < 16; i++) {
            if (!curRoot || !curRoot.elementFromPoint) break;
            const el = curRoot.elementFromPoint(x, y);
            if (!el) break;
            chain.push(el);
            if (el.shadowRoot) {
              curRoot = el.shadowRoot;
            } else {
              break;
            }
          }

          const ancestors = [];
          for (const leaf of chain.reverse()) {
            let node = leaf;
            for (let depth = 0; node && depth < 24; depth++) {
              if (node instanceof HTMLElement) ancestors.push(node);
              if (node.parentElement) {
                node = node.parentElement;
              } else {
                const rn = node.getRootNode ? node.getRootNode() : null;
                node = rn && rn.host ? rn.host : null;
              }
            }
          }

          for (const el of ancestors) {
            if (canScrollByWrite(el)) return el;
          }
        } catch (_) {}
        return null;
      };

      const centerCandidate = centerPointChainCandidate();
      if (centerCandidate) {
        globalThis.__asLog('selected element center-chain', this.describeElement(centerCandidate));
        return centerCandidate;
      }

      const walk = (node) => {
        if (!node || !node.querySelectorAll) return;
        let elements = [];
        try { elements = Array.from(node.querySelectorAll('*')); } catch (_) {}
        for (const el of elements) {
          if (!el || seen.has(el)) continue;
          seen.add(el);
          candidates.push(el);
          try { if (el.shadowRoot) walk(el.shadowRoot); } catch (_) {}
        }
      };

      walk(document);

      let best = null;
      let bestScore = 0;
      for (const el of candidates) {
        if (!(el instanceof HTMLElement)) continue;
        const style = window.getComputedStyle(el);
        const overflowY = style.overflowY;
        const overflowLooksScrollable = overflowY === 'auto' || overflowY === 'scroll' || overflowY === 'overlay';
        const scrollable = el.scrollHeight - el.clientHeight;
        const writable = canScrollByWrite(el);
        if (scrollable <= 8 && !overflowLooksScrollable && !writable) continue;
        const rect = el.getBoundingClientRect();
        const visibleHeight = Math.max(0, Math.min(rect.bottom, window.innerHeight) - Math.max(rect.top, 0));
        if (visibleHeight < 48) continue;
        const score = scrollable * 0.75 + visibleHeight * 0.25 + (overflowLooksScrollable ? 120 : 0) + (writable ? 220 : 0);
        if (score > bestScore) {
          best = el;
          bestScore = score;
        }
      }

      return best || root;
    },

    resolveElement() {
      if (!this.scrollEl || !this.scrollEl.isConnected) {
        this.scrollEl = this.pickScrollableElement();
        globalThis.__asLog('selected element', this.describeElement(this.scrollEl));
      }
      return this.scrollEl;
    },

    windowMetrics() {
      const root = document.scrollingElement || document.documentElement || document.body;
      const docEl = document.documentElement || root;
      const body = document.body || root;
      const viewport = window.innerHeight || docEl.clientHeight || 0;
      const top = window.pageYOffset ?? root.scrollTop ?? 0;
      const height = Math.max(root.scrollHeight || 0, docEl.scrollHeight || 0, body.scrollHeight || 0);
      return { top, viewport, height };
    },

    elementMetrics(el) {
      const target = el || this.resolveElement();
      return {
        top: target && target.scrollTop ? target.scrollTop : 0,
        viewport: target && target.clientHeight ? target.clientHeight : 0,
        height: target && target.scrollHeight ? target.scrollHeight : 0,
      };
    },

    scrollWindow(delta, ts) {
      const before = this.windowMetrics();
      const max = Math.max(0, before.height - before.viewport);
      if (before.top >= max - 1 && max > 0) {
        if (this.returnDelayMs > 0 && ts < this.holdUntilTs) {
          return true;
        }
        const a = this.adv();
        if (a && a.atEnd(ts, MIN_PASS_MS)) return true;
        window.scrollTo(0, 0);
        this.holdUntilTs = ts + this.topDelayMs;
        return true;
      }
      window.scrollBy(0, delta);
      const after = this.windowMetrics();
      return Math.abs(after.top - before.top) > 0.5;
    },

    scrollElement(delta, ts) {
      const el = this.resolveElement();
      if (!el || this.isRoot(el)) return false;
      const before = this.elementMetrics(el);
      const max = Math.max(0, before.height - before.viewport);
      if (before.top >= max - 1 && max > 0) {
        if (this.returnDelayMs > 0 && ts < this.holdUntilTs) {
          return true;
        }
        const a = this.adv();
        if (a && a.atEnd(ts, MIN_PASS_MS)) return true;
        el.scrollTop = 0;
        this.holdUntilTs = ts + this.topDelayMs;
        return true;
      }
      el.scrollTop = before.top + delta;
      const after = this.elementMetrics(el);
      return Math.abs(after.top - before.top) > 0.5;
    },

    tick(ts) {
      if (!this.enabled) {
        this.raf = 0;
        return;
      }

      if (this.mode === 'step') {
        this.tickStep(ts);
        this.raf = requestAnimationFrame((nextTs) => this.tick(nextTs));
        return;
      }

      if (!this.lastTs) this.lastTs = ts;
      if (this.holdUntilTs > ts) {
        const held = this.adv();
        if (held) held.alive(ts);
        this.raf = requestAnimationFrame((nextTs) => this.tick(nextTs));
        return;
      }

      const dt = Math.max(0, Math.min(0.15, (ts - this.lastTs) / 1000));
      this.lastTs = ts;
      const delta = this.pxPerSec * dt;

      let moved = false;
      if (this.mode === 'window') {
        moved = this.scrollWindow(delta, ts);
      } else if (this.mode === 'element') {
        moved = this.scrollElement(delta, ts);
      } else {
        moved = this.scrollWindow(delta, ts);
        if (!moved) moved = this.scrollElement(delta, ts);
      }

      if (moved) {
        const wm = this.windowMetrics();
        const windowMax = Math.max(0, wm.height - wm.viewport);
        const em = this.elementMetrics(this.scrollEl);
        const elementMax = Math.max(0, em.height - em.viewport);
        const atBottom = (windowMax > 0 && wm.top >= windowMax - 1)
          || (elementMax > 0 && em.top >= elementMax - 1);
        if (atBottom && this.returnDelayMs > 0) {
          this.holdUntilTs = ts + this.returnDelayMs;
        }
      }

      this.repickCounter += 1;
      if (!moved || this.repickCounter >= 180) {
        this.scrollEl = this.pickScrollableElement();
        this.repickCounter = 0;
      }

      const a = this.adv();
      if (a) {
        if (moved) {
          a.alive(ts);
        } else if (this.nothingToScroll()) {
          // A page that fits the screen is at its end at once: a pass per
          // top-and-bottom delay, and never faster than MIN_PASS_MS.
          a.atEnd(ts, Math.max(MIN_PASS_MS, this.topDelayMs + this.returnDelayMs));
        }
      }

      this.raf = requestAnimationFrame((nextTs) => this.tick(nextTs));
    },

    tickStep(ts) {
      if (this.nextStepTs > ts) {
        const waiting = this.adv();
        if (waiting) waiting.alive(ts);
        return;
      }

      const stepPx = Number.isFinite(this.stepPx) && this.stepPx > 0 ? this.stepPx : (window.innerHeight || 900);
      const stepTimeMs = Number.isFinite(this.stepTimeMs) && this.stepTimeMs > 0 ? this.stepTimeMs : 0;
      const stepDelayMs = Number.isFinite(this.stepDelayMs) && this.stepDelayMs >= 0 ? this.stepDelayMs : 0;

      const windowStep = () => {
        const wm = this.windowMetrics();
        const max = Math.max(0, wm.height - wm.viewport);
        if (max <= 1) return false;

        if (wm.top >= max - 1) {
          const a = this.adv();
          if (a && a.atEnd(ts, MIN_PASS_MS)) {
            this.nextStepTs = ts + 250;
            return true;
          }
          window.scrollTo({ top: 0, behavior: 'auto' });
          this.nextStepTs = ts + stepDelayMs;
          return true;
        }

        const target = Math.min(max, wm.top + stepPx);
        window.scrollTo({ top: target, behavior: stepTimeMs > 0 ? 'smooth' : 'auto' });
        const stepped = this.adv();
        if (stepped) stepped.alive(ts);
        this.nextStepTs = ts + stepDelayMs + stepTimeMs;
        return true;
      };

      const elementStep = () => {
        const el = this.resolveElement();
        if (!el || this.isRoot(el)) return false;

        const em = this.elementMetrics(el);
        const max = Math.max(0, em.height - em.viewport);
        if (max <= 1) return false;

        if (em.top >= max - 1) {
          const a = this.adv();
          if (a && a.atEnd(ts, MIN_PASS_MS)) {
            this.nextStepTs = ts + 250;
            return true;
          }
          el.scrollTo({ top: 0, behavior: 'auto' });
          this.nextStepTs = ts + stepDelayMs;
          return true;
        }

        const target = Math.min(max, em.top + stepPx);
        el.scrollTo({ top: target, behavior: stepTimeMs > 0 ? 'smooth' : 'auto' });
        const stepped = this.adv();
        if (stepped) stepped.alive(ts);
        this.nextStepTs = ts + stepDelayMs + stepTimeMs;
        return true;
      };

      let moved = false;
      if (this.mode === 'window') {
        moved = windowStep();
      } else if (this.mode === 'element') {
        moved = elementStep();
      } else {
        moved = windowStep();
        if (!moved) moved = elementStep();
      }

      if (!moved) {
        this.scrollEl = this.pickScrollableElement();
        this.nextStepTs = ts + stepDelayMs;
        const a = this.adv();
        if (a && this.nothingToScroll()) a.atEnd(ts, Math.max(MIN_PASS_MS, stepDelayMs));
      }
    },

    enable() {
      if (this.enabled) return;
      this.enabled = true;
      this.lastTs = 0;
      this.holdUntilTs = performance.now() + this.topDelayMs;
      this.nextStepTs = performance.now() + Math.max(250, this.stepDelayMs);
      this.raf = requestAnimationFrame((ts) => this.tick(ts));
      globalThis.__asLog('enabled', 'mode', this.mode, 'speed', this.pxPerSec, 'stepPx', this.stepPx, 'stepTimeMs', this.stepTimeMs, 'stepDelayMs', this.stepDelayMs, 'topDelayMs', this.topDelayMs, 'returnDelayMs', this.returnDelayMs);
    },

    disable() {
      this.enabled = false;
      this.lastTs = 0;
      this.holdUntilTs = 0;
      this.nextStepTs = 0;
      if (this.raf) cancelAnimationFrame(this.raf);
      this.raf = 0;
      globalThis.__asLog('disabled');
    },

    setSpeed(pxPerSec) {
      const n = Number(pxPerSec);
      if (!Number.isFinite(n) || n <= 0) return;
      this.pxPerSec = n;
      globalThis.__asLog('setSpeed', n);
    },

    setMode(mode) {
      if (mode === 'window' || mode === 'element' || mode === 'auto' || mode === 'step') {
        this.mode = mode;
      } else {
        this.mode = 'auto';
      }
      globalThis.__asLog('setMode', this.mode);
    },

    setStepOptions(stepPx, stepTimeMs, stepDelayMs) {
      const px = Number(stepPx);
      const time = Number(stepTimeMs);
      const delay = Number(stepDelayMs);
      this.stepPx = Number.isFinite(px) && px > 0 ? px : 900;
      this.stepTimeMs = Number.isFinite(time) && time > 0 ? time : 0;
      this.stepDelayMs = Number.isFinite(delay) && delay >= 0 ? delay : 2000;
      globalThis.__asLog('setStepOptions', this.stepPx, this.stepTimeMs, this.stepDelayMs);
    },

    setTopDelay(ms) {
      const n = Number(ms);
      this.topDelayMs = Number.isFinite(n) && n > 0 ? n : 0;
      globalThis.__asLog('setTopDelay', this.topDelayMs);
    },

    setReturnDelay(ms) {
      const n = Number(ms);
      this.returnDelayMs = Number.isFinite(n) && n > 0 ? n : 0;
      globalThis.__asLog('setReturnDelay', this.returnDelayMs);
    },

    state() {
      const el = this.resolveElement();
      const wm = this.windowMetrics();
      const em = this.elementMetrics(el);
      const pendingConnections = (globalThis.__asNetwork && Number.isFinite(globalThis.__asNetwork.pending))
        ? globalThis.__asNetwork.pending
        : 0;
      const idleForMs = globalThis.__asNetwork && globalThis.__asNetwork.idleForMs
        ? globalThis.__asNetwork.idleForMs()
        : 0;
      return {
        enabled: this.enabled,
        mode: this.mode,
        pxPerSec: this.pxPerSec,
        stepPx: this.stepPx,
        stepTimeMs: this.stepTimeMs,
        stepDelayMs: this.stepDelayMs,
        topDelayMs: this.topDelayMs,
        returnDelayMs: this.returnDelayMs,
        holdForMs: Math.max(0, this.holdUntilTs - performance.now()),
        element: this.describeElement(el),
        window: wm,
        elementMetrics: em,
        pendingConnections,
        idleForMs,
      };
    },
  };

  globalThis.__as = api;

  globalThis.__asBroadcast = (payload) => {
    let len = 0;
    try { len = window.frames.length || 0; } catch (_) { len = 0; }
    for (let i = 0; i < len; i++) {
      try {
        window.frames[i].postMessage({ __asCmd: 'apply', payload }, '*');
      } catch (_) {}
    }
  };

  globalThis.__asApply = (payload) => {
    if (!payload || !globalThis.__as) return false;
    try {
      const st = globalThis.__as.state ? globalThis.__as.state() : null;
      globalThis.__asLog(
        'apply begin',
        'mode', payload.mode,
        'speed', payload.speed,
        'enable', payload.enable,
        'topDelay', payload.topDelay,
        'returnDelay', payload.returnDelay,
        'stepPx', payload.stepPx,
        'stepTime', payload.stepTime,
        'stepDelay', payload.stepDelay,
        'pending', st && st.pendingConnections,
        'idleForMs', st && st.idleForMs,
        'element', st && st.element,
      );
    } catch (_) {}

    globalThis.__as.setMode(payload.mode || 'auto');
    globalThis.__as.setSpeed(payload.speed || 120);
    if (globalThis.__as.setStepOptions) globalThis.__as.setStepOptions(payload.stepPx, payload.stepTime, payload.stepDelay);
    if (globalThis.__as.setTopDelay) globalThis.__as.setTopDelay(payload.topDelay || 0);
    if (globalThis.__as.setReturnDelay) globalThis.__as.setReturnDelay(payload.returnDelay || 0);
    if (payload.enable) globalThis.__as.enable(); else globalThis.__as.disable();

    try {
      setTimeout(() => {
        try {
          const delayed = globalThis.__as.state ? globalThis.__as.state() : null;
          globalThis.__asLog(
            'apply +3s probe',
            'pending', delayed && delayed.pendingConnections,
            'idleForMs', delayed && delayed.idleForMs,
            'element', delayed && delayed.element,
            'windowTop', delayed && delayed.window && delayed.window.top,
            'windowMax', delayed && delayed.window ? Math.max(0, (delayed.window.height || 0) - (delayed.window.viewport || 0)) : null,
            'elementTop', delayed && delayed.elementMetrics && delayed.elementMetrics.top,
            'elementMax', delayed && delayed.elementMetrics ? Math.max(0, (delayed.elementMetrics.height || 0) - (delayed.elementMetrics.viewport || 0)) : null,
          );
        } catch (_) {}
      }, 3000);
    } catch (_) {}

    if (!payload.__fromParent && globalThis.__asBroadcast) {
      globalThis.__asBroadcast({ ...payload, __fromParent: true });
    }
    return true;
  };

  if (!globalThis.__asMessageListenerInstalled) {
    window.addEventListener('message', (ev) => {
      const data = ev && ev.data;
      if (!data || data.__asCmd !== 'apply' || !data.payload) return;
      if (globalThis.__asApply) globalThis.__asApply({ ...data.payload, __fromParent: true });
    });
    globalThis.__asMessageListenerInstalled = true;
  }

  globalThis.__asLog('installed v3');
})();
