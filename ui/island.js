// Lucent island: shows the Windows media session and drives the island window size.
// Event-driven. Timers: a one-shot timeout for the auto-peek, and a progress
// tick for the timeline that runs only while the card is expanded and playing.
(function () {
  'use strict';

  const PEEK_MS = 3000;
  const SHRINK_FALLBACK_MS = 600;
  const TIMELINE_TICK_MS = 250;
  const tauri = window.__TAURI__;
  const island = document.getElementById('island');
  const el = {
    miniThumb: document.getElementById('mini-thumb'),
    miniImg: document.getElementById('mini-img'),
    thumb: document.getElementById('thumb'),
    img: document.getElementById('img'),
    title: document.getElementById('title'),
    artist: document.getElementById('artist'),
    elapsed: document.getElementById('elapsed'),
    remaining: document.getElementById('remaining'),
    track: document.getElementById('track'),
    fill: document.getElementById('fill'),
    prev: document.getElementById('prev'),
    toggle: document.getElementById('toggle'),
    next: document.getElementById('next')
  };

  const state = {
    expanded: false,
    hovered: false,
    pendingShrink: false,
    shrinkTimer: 0,
    peekTimer: 0,
    emptyPending: false,
    emptyTimer: 0,
    lastTitle: null,
    // Last sample from the backend. position is in ms as of updatedAt (epoch ms).
    timeline: { position: 0, duration: null, updatedAt: 0, playing: false }
  };
  let timelineTimer = 0;

  // Window resizes and hit-region updates run one at a time, in order.
  // Only enqueue() reads or reassigns resizeChain. Step callbacks never do, so a
  // step can't end up waiting on the chain it is part of.
  let resizeChain = Promise.resolve();

  function invoke(cmd, args) {
    if (!tauri || !tauri.core) {
      return Promise.reject(new Error('Tauri API unavailable'));
    }
    return tauri.core.invoke(cmd, args);
  }

  function enqueue(step, label) {
    resizeChain = resizeChain
      .then(step)
      .catch((err) => console.warn(label + ' failed', err));
    return resizeChain;
  }

  // The native hit region clips the window to the drawn pill, so clicks outside it
  // reach the windows below. Arguments are computed when the step runs, so queued
  // calls see the latest state. While the empty capsule is still shrinking, the old
  // (larger) region is kept.
  function regionStep() {
    return invoke('set_hit_region', {
      expanded: state.expanded,
      empty: island.classList.contains('is-empty') && !state.emptyPending
    });
  }

  function applyRegion() {
    return enqueue(regionStep, 'set_hit_region');
  }

  // Resize the native window to match the pill's new size, then its hit region.
  function resizeWindow(expanded) {
    return enqueue(() => invoke('set_expanded', { expanded: expanded })
      .catch((err) => console.warn('set_expanded failed', err))
      .then(regionStep), 'resize');
  }

  // Growing the region is immediate. Shrinking to the empty capsule waits until the
  // pill's height transition ends, so the drawn pill is never clipped.
  function onEmptyChanged(nowEmpty) {
    if (nowEmpty && !state.expanded && transitionsEnabled()) {
      state.emptyPending = true;
      clearTimeout(state.emptyTimer);
      state.emptyTimer = setTimeout(settleEmpty, SHRINK_FALLBACK_MS);
    } else {
      state.emptyPending = false;
      clearTimeout(state.emptyTimer);
      state.emptyTimer = 0;
      applyRegion();
    }
  }

  function settleEmpty() {
    clearTimeout(state.emptyTimer);
    state.emptyTimer = 0;
    if (!state.emptyPending) return;
    state.emptyPending = false;
    applyRegion();
  }

  function transitionsEnabled() {
    return getComputedStyle(island).transitionDuration
      .split(',')
      .some((d) => parseFloat(d) > 0);
  }

  // Current playhead in ms. Extrapolates from the last sample while playing.
  function timelinePositionMs(tl, now) {
    let pos = tl.position;
    if (tl.playing) pos += now - tl.updatedAt;
    return Math.min(Math.max(pos, 0), tl.duration);
  }

  // m:ss, or h:mm:ss from one hour up.
  function formatTime(ms) {
    const secs = Math.floor(Math.max(ms, 0) / 1000);
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = String(secs % 60).padStart(2, '0');
    return h > 0 ? h + ':' + String(m).padStart(2, '0') + ':' + s : m + ':' + s;
  }

  // Draws the timeline from the last sample. Hidden when the length is unknown.
  function renderTimeline() {
    const tl = state.timeline;
    const total = tl.duration;
    island.classList.toggle('has-timeline', total !== null);
    if (total === null) return;
    const pos = timelinePositionMs(tl, Date.now());
    const elapsed = formatTime(pos);
    el.fill.style.transform = 'scaleX(' + pos / total + ')';
    el.elapsed.textContent = elapsed;
    el.remaining.textContent = '-' + formatTime(total - pos);
    el.track.setAttribute('aria-valuemax', String(total));
    el.track.setAttribute('aria-valuenow', String(Math.round(pos)));
    el.track.setAttribute('aria-valuetext', elapsed + ' of ' + formatTime(total));
  }

  // The only timeline timer. Runs while the card is open and a track is playing.
  // Started and stopped from expand, collapse and render.
  function syncTimeline() {
    const active = state.expanded && state.timeline.playing && state.timeline.duration !== null;
    if (active && !timelineTimer) {
      timelineTimer = setInterval(renderTimeline, TIMELINE_TICK_MS);
    } else if (!active && timelineTimer) {
      clearInterval(timelineTimer);
      timelineTimer = 0;
    }
  }

  // Window grows first, then the pill animates into the new space.
  function expand() {
    if (state.expanded) return;
    state.expanded = true;
    state.pendingShrink = false;
    clearTimeout(state.shrinkTimer);
    state.shrinkTimer = 0;
    renderTimeline();
    syncTimeline();
    resizeWindow(true).then(() => {
      if (state.expanded) island.classList.add('expanded');
    });
  }

  // The pill shrinks first. The window shrinks once the height transition ends.
  function collapse() {
    if (!state.expanded) return;
    state.expanded = false;
    syncTimeline();
    const wasShown = island.classList.contains('expanded');
    island.classList.remove('expanded');
    if (wasShown && transitionsEnabled()) {
      state.pendingShrink = true;
      // One-shot fallback in case no transition event ever fires.
      clearTimeout(state.shrinkTimer);
      state.shrinkTimer = setTimeout(shrink, SHRINK_FALLBACK_MS);
    } else {
      state.pendingShrink = false;
      resizeWindow(false);
    }
  }

  // Shrinks the native window if a shrink is pending; otherwise a no-op.
  function shrink() {
    clearTimeout(state.shrinkTimer);
    state.shrinkTimer = 0;
    if (!state.pendingShrink) return;
    state.pendingShrink = false;
    resizeWindow(false);
  }

  function onShrinkTransition(event) {
    if (event.target !== island || event.propertyName !== 'height') return;
    shrink();
    settleEmpty();
  }
  island.addEventListener('transitionend', onShrinkTransition);
  island.addEventListener('transitioncancel', onShrinkTransition);

  function cancelPeek() {
    clearTimeout(state.peekTimer);
    state.peekTimer = 0;
  }

  // Briefly show the card when the track changes.
  function peek() {
    if (state.hovered) return;
    cancelPeek();
    expand();
    state.peekTimer = setTimeout(() => {
      state.peekTimer = 0;
      if (!state.hovered) collapse();
    }, PEEK_MS);
  }

  function setArt(thumb, img, url) {
    if (url) {
      if (img.getAttribute('src') !== url) img.src = url;
      thumb.classList.add('has-art');
    } else {
      img.removeAttribute('src');
      thumb.classList.remove('has-art');
    }
  }

  function render(media, announce) {
    const title = media.title || null;
    const playing = media.playing === true;

    const wasEmpty = island.classList.contains('is-empty');
    island.classList.toggle('is-empty', !title);
    if (wasEmpty !== !title) onEmptyChanged(!title);
    island.classList.toggle('is-playing', playing);
    setArt(el.miniThumb, el.miniImg, media.artwork);
    setArt(el.thumb, el.img, media.artwork);
    el.title.textContent = title || 'Nothing playing';
    el.artist.textContent = media.artist || media.app || '';
    el.toggle.setAttribute('aria-label', playing ? 'Pause' : 'Play');

    const changed = title !== state.lastTitle;
    state.lastTitle = title;

    const tl = state.timeline;
    tl.duration = typeof media.duration_ms === 'number' && media.duration_ms > 0 ? media.duration_ms : null;
    tl.position = typeof media.position_ms === 'number' ? media.position_ms : 0;
    // Without a sample time, the time this update arrived is the best guess.
    tl.updatedAt = typeof media.updated_at_ms === 'number' ? media.updated_at_ms : Date.now();
    tl.playing = playing;
    renderTimeline();
    syncTimeline();

    if (announce && changed && title) peek();
  }

  function command(name) {
    invoke(name).catch((err) => console.warn(name + ' failed', err));
  }

  el.prev.addEventListener('click', () => command('media_prev'));
  el.toggle.addEventListener('click', () => command('media_toggle'));
  el.next.addEventListener('click', () => command('media_next'));

  island.addEventListener('mouseenter', () => {
    state.hovered = true;
    cancelPeek();
    expand();
  });

  island.addEventListener('mouseleave', () => {
    state.hovered = false;
    cancelPeek();
    collapse();
  });

  async function start() {
    // The window starts collapsed and the page starts empty: clip to the capsule now.
    applyRegion();
    if (!tauri || !tauri.event) {
      render({}, false);
      return;
    }
    // Listen first so no update is missed between the initial read and now.
    try {
      await tauri.event.listen('media://update', (event) => {
        render(event.payload || {}, true);
      });
    } catch (err) {
      console.warn('media listener failed', err);
    }
    try {
      render((await invoke('get_media_state')) || {}, false);
    } catch (err) {
      console.warn('get_media_state failed', err);
      render({}, false);
    }
  }

  start();
})();
