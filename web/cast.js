// Shared WebRTC plumbing for the cast sender (cast.html) and the display
// receiver (cast_display.html).
//
// The server relays opaque `{sdp}` / `{ice}` blobs between exactly two peers, so
// there are no rooms and no peer ids here. That is the main simplification over
// picklecast, whose display had to defend against duplicate offers arriving from
// several public trackers at once.
//
// The sender always makes the offer, because it is the side holding the media.

(function (global) {
  'use strict';

  // --- signaling socket -----------------------------------------------------

  // handlers: {welcome, peer, signal, error, closed}
  //
  // `ticket` comes from POST /api/cast/claim. The socket carries no code: the
  // sender is authorised once, at claim time, before the screen picker opens.
  function openSocket(role, ticket, handlers) {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let url = `${proto}//${location.host}/api/cast/ws?role=${encodeURIComponent(role)}`;
    if (ticket) url += `&ticket=${encodeURIComponent(ticket)}`;

    const ws = new WebSocket(url);
    let closedByUs = false;

    ws.onmessage = (event) => {
      let frame;
      try {
        frame = JSON.parse(event.data);
      } catch (_) {
        return;
      }
      if (frame.type === 'welcome') handlers.welcome && handlers.welcome(frame);
      else if (frame.type === 'peer') handlers.peer && handlers.peer(frame.connected);
      else if (frame.type === 'signal') handlers.signal && handlers.signal(frame.data);
      else if (frame.type === 'pairing') handlers.pairing && handlers.pairing(frame);
      else if (frame.type === 'error') handlers.error && handlers.error(frame);
    };

    ws.onclose = () => {
      if (!closedByUs) handlers.closed && handlers.closed();
    };

    return {
      signal(data) {
        if (ws.readyState === WebSocket.OPEN)
          ws.send(JSON.stringify({ type: 'signal', data }));
      },
      stop() {
        if (ws.readyState === WebSocket.OPEN)
          ws.send(JSON.stringify({ type: 'stop' }));
      },
      close() {
        closedByUs = true;
        try { ws.close(); } catch (_) { /* already gone */ }
      },
      get ready() {
        return ws.readyState === WebSocket.OPEN;
      },
    };
  }

  // --- peer connection ------------------------------------------------------

  // Wraps RTCPeerConnection with the two bits of bookkeeping that are easy to get
  // wrong: buffering ICE candidates that arrive before the remote description is
  // set, and tearing the whole thing down exactly once.
  function createPeer(iceServers, socket, handlers) {
    const pc = new RTCPeerConnection({ iceServers: iceServers || [] });
    let pendingCandidates = [];
    let closed = false;

    pc.onicecandidate = (event) => {
      if (event.candidate) socket.signal({ ice: event.candidate });
    };

    pc.ontrack = (event) => handlers.track && handlers.track(event);

    const onDead = () => {
      const state = pc.connectionState;
      if (state === 'failed' || state === 'closed' || state === 'disconnected')
        handlers.dead && handlers.dead(state);
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'connected') handlers.connected && handlers.connected();
      onDead();
    };
    pc.oniceconnectionstatechange = () => {
      if (pc.iceConnectionState === 'failed') handlers.dead && handlers.dead('ice-failed');
    };

    async function flushCandidates() {
      const queued = pendingCandidates;
      pendingCandidates = [];
      for (const candidate of queued) {
        try {
          await pc.addIceCandidate(new RTCIceCandidate(candidate));
        } catch (e) {
          console.warn('addIceCandidate failed', e);
        }
      }
    }

    return {
      pc,
      async applyRemote(description) {
        await pc.setRemoteDescription(new RTCSessionDescription(description));
        await flushCandidates();
      },
      async addCandidate(candidate) {
        // Candidates routinely beat the SDP through the relay; adding one before
        // the remote description exists throws and loses it for good.
        if (!pc.remoteDescription) {
          pendingCandidates.push(candidate);
          return;
        }
        try {
          await pc.addIceCandidate(new RTCIceCandidate(candidate));
        } catch (e) {
          console.warn('addIceCandidate failed', e);
        }
      },
      close() {
        if (closed) return;
        closed = true;
        pendingCandidates = [];
        try { pc.close(); } catch (_) { /* already closed */ }
      },
    };
  }

  // --- video element --------------------------------------------------------

  // Attach a remote stream, preferring unmuted playback.
  //
  // Two traps, both hit in practice:
  //  * `ontrack` fires once per track (video and audio) for the same stream. The
  //    second play() aborts the first with AbortError, and a naive catch then
  //    re-mutes a stream that was playing fine -- so ignore repeats.
  //  * Autoplay with sound needs a user gesture unless the kiosk browser was
  //    started with --autoplay-policy=no-user-gesture-required. Fall back to
  //    muted rather than not playing at all.
  function attachStream(video, stream, onUnmuted) {
    if (video.srcObject === stream) return;
    video.srcObject = stream;
    video.muted = false;

    const playing = video.play();
    if (!playing || !playing.catch) return;

    playing
      .then(() => {
        if (!video.muted && onUnmuted) onUnmuted();
      })
      .catch(() => {
        video.muted = true;
        video.play().catch((e) => console.warn('muted playback failed', e));
      });
  }

  global.Cast = { openSocket, createPeer, attachStream };
})(window);
