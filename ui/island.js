// Lucent island: shows the Windows media session and drives the island window size.
// Event-driven. The only timer is a one-shot timeout for the auto-peek.
(function () {
  'use strict';

  const PEEK_MS = 3000;
  const SHRINK_FALLBACK_MS = 600;
  const tauri = window.__TAURI__;
  const island = document.getElementById('island');
  const el = {
    miniThumb: document.getElementById('mini-thumb'),
    miniImg: document.getElementById('mini-img'),
    thumb: document.getElementById('thumb'),
    img: document.getElementById('img'),
    title: document.getElementById('title'),
    artist: document.getElementById('artist'),
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
    lastTitle: null
  };

  // Window resizes run one at a time, in order.
  let resizeChain = Promise.resolve();

  function invoke(cmd, args) {
    if (!tauri || !tauri.core) {
      return Promise.reject(new Error('Tauri API unavailable'));
    }
    return tauri.core.invoke(cmd, args);
  }

  // Resize the native window to match the pill's new size.
  function resizeWindow(expanded) {
    resizeChain = resizeChain
      .then(() => invoke('set_expanded', { expanded: expanded }))
      .catch((err) => console.warn('set_expanded failed', err));
    return resizeChain;
  }

  function transitionsEnabled() {
    return getComputedStyle(island).transitionDuration
      .split(',')
      .some((d) => parseFloat(d) > 0);
  }

  // Window grows first, then the pill animates into the new space.
  function expand() {
    if (state.expanded) return;
    state.expanded = true;
    state.pendingShrink = false;
    clearTimeout(state.shrinkTimer);
    state.shrinkTimer = 0;
    resizeWindow(true).then(() => {
      if (state.expanded) island.classList.add('expanded');
    });
  }

  // The pill shrinks first. The window shrinks once the height transition ends.
  function collapse() {
    if (!state.expanded) return;
    state.expanded = false;
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

    island.classList.toggle('is-empty', !title);
    island.classList.toggle('is-playing', playing);
    setArt(el.miniThumb, el.miniImg, media.artwork);
    setArt(el.thumb, el.img, media.artwork);
    el.title.textContent = title || 'Nothing playing';
    el.artist.textContent = media.artist || media.app || '';
    el.toggle.setAttribute('aria-label', playing ? 'Pause' : 'Play');

    const changed = title !== state.lastTitle;
    state.lastTitle = title;
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
