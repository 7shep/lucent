// Lucent flyout: one page, four panels (start, search, quick settings,
// notifications). The window is the open panel plus 24 px on every side. A panel
// springs in from its taskbar icon on flyout-show and back out on flyout-close.
// While it moves, the native hit regions follow its rectangles every frame.
// Plain IIFE, no build step. Text is always set with textContent.
(function () {
  'use strict';

  const tauri = window.__TAURI__;
  const PAD = 24;
  const ICON_PX = 32;
  const BIG_ICON_PX = 48;
  const SEARCH_DEBOUNCE_MS = 120;
  const SETTLE_MS = 800; // fallback if transitionend never arrives
  const REDUCED_SETTLE_MS = 220;
  const CATEGORY_ORDER = ['Productivity', 'Creativity', 'Social', 'Games', 'Developer tools', 'Utilities', 'Other'];
  const PANEL_IDS = {
    start: 'p-start',
    search: 'p-search',
    quick_settings: 'p-quick_settings',
    notifications: 'p-notifications',
    tray: 'p-tray'
  };
  // ask: the confirm question. Those items need a second click; the rest act at once.
  const POWER_ITEMS = [
    { label: 'Lock', action: 'lock' },
    { label: 'Sign out', action: 'signout' },
    { label: 'Sleep', action: 'sleep' },
    { label: 'Restart', action: 'restart', ask: 'Restart now?' },
    { label: 'Shut down', action: 'shutdown', ask: 'Shut down now?' }
  ];
  const BD_PAD = 80; // keep in step with --bd-pad in glass.css
  const BD_STYLES = ['fill', 'fit', 'stretch', 'tile', 'center', 'span'];
  const SVG_NS = 'http://www.w3.org/2000/svg';
  const reducedQuery = window.matchMedia ? window.matchMedia('(prefers-reduced-motion: reduce)') : null;

  function reduced() {
    return !!(reducedQuery && reducedQuery.matches);
  }

  function $(id) {
    return document.getElementById(id);
  }

  const el = {
    startQ: $('start-q'),
    allScroll: $('all-scroll'),
    startHome: $('start-home'),
    startResults: $('start-results'),
    pins: $('pins'),
    recent: $('recent'),
    cats: $('cats'),
    catView: $('cat-view'),
    catBack: $('cat-back'),
    catName: $('cat-name'),
    catList: $('cat-list'),
    avatar: $('avatar'),
    avatarGlyph: $('avatar-glyph'),
    userName: $('user-name'),
    powerBtn: $('power-btn'),
    searchQ: $('search-q'),
    searchResults: $('search-results'),
    qsWifi: $('qs-wifi'),
    qsWifiLabel: $('qs-wifi-label'),
    qsBt: $('qs-bt'),
    qsFocus: $('qs-focus'),
    qsNight: $('qs-night'),
    qsAir: $('qs-air'),
    qsCast: $('qs-cast'),
    bright: $('bright'),
    rowBright: $('row-bright'),
    vol: $('vol'),
    rowVol: $('row-vol'),
    volBtn: $('vol-btn'),
    qsStatus: $('qs-status'),
    qsStatusText: $('qs-status-text'),
    qsSettings: $('qs-settings'),
    ntfOpen: $('ntf-open'),
    calToday: $('cal-today'),
    calMonth: $('cal-month'),
    calPrev: $('cal-prev'),
    calNext: $('cal-next'),
    cal: $('cal'),
    trayGrid: $('tray-grid'),
    trayEmpty: $('tray-empty'),
    menu: $('menu'),
    menuItems: $('menu-items')
  };

  const state = {
    panel: null, // the open (or opening) panel name
    session: 0, // bumps on every show and close; stale callbacks compare it
    anim: 0,
    settleTimer: 0,
    endNode: null,
    endFn: null,
    regionKey: '',
    pins: [],
    apps: [],
    groups: new Map(),
    system: {},
    cal: { year: 0, month: 0 },
    windowX: 0, // the flyout window's top-left on the monitor (flyout-show)
    windowY: 0
  };

  // ---- Tauri access -------------------------------------------------------

  function invoke(cmd, args) {
    if (!tauri || !tauri.core) {
      return Promise.reject(new Error('Tauri API unavailable'));
    }
    try {
      return Promise.resolve(tauri.core.invoke(cmd, args));
    } catch (err) {
      return Promise.reject(err);
    }
  }

  // Never rejects: failures are logged and resolve to null.
  function call(cmd, args) {
    return invoke(cmd, args).catch((err) => {
      console.warn(cmd + ' failed', err);
      return null;
    });
  }

  function listen(name, fn) {
    if (!tauri || !tauri.event) return;
    tauri.event.listen(name, fn).catch((err) => console.warn('listen ' + name + ' failed', err));
  }

  function closeFlyout() {
    call('flyout_request_close');
  }

  // ---- DOM helpers --------------------------------------------------------

  // h(tag, props, ...children). Strings become text nodes.
  function h(tag, props) {
    const node = document.createElement(tag);
    if (props) {
      for (const key of Object.keys(props)) {
        if (key === 'text') node.textContent = props[key];
        else if (key === 'class') node.className = props[key];
        else node.setAttribute(key, props[key]);
      }
    }
    for (let i = 2; i < arguments.length; i++) {
      const kid = arguments[i];
      if (kid === null || kid === undefined || kid === false) continue;
      node.appendChild(typeof kid === 'string' ? document.createTextNode(kid) : kid);
    }
    return node;
  }

  // Sprite glyph (inline SVG <use>), e.g. glyph('file').
  function glyph(name, cls) {
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', cls || 'g');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('focusable', 'false');
    const use = document.createElementNS(SVG_NS, 'use');
    use.setAttribute('href', '#i-' + name);
    svg.appendChild(use);
    return svg;
  }

  function note(text, wide) {
    return h('div', { class: wide === false ? 'note' : 'note wide', text: text });
  }

  function nameSpan(text) {
    return h('span', { class: 'row-name', text: text });
  }

  // ---- Backdrop: the wallpaper behind each glass surface ------------------
  // Each .glass element gets a .bd layer (monitor size plus BD_PAD on every side,
  // blurred in CSS) holding a .bd-img that is exactly the monitor. Its offset is
  // minus the surface's monitor position: the window's (windowX, windowY) from
  // flyout-show plus the surface's offset inside the window.

  const bd = { mw: 0, mh: 0, image: null, style: 'fill', color: '#000000', token: 0 };
  const bdSurfaces = [];

  function makeBackdropLayer(node) {
    const layer = document.createElement('div');
    layer.className = 'bd';
    layer.setAttribute('aria-hidden', 'true');
    const img = document.createElement('div');
    img.className = 'bd-img';
    layer.appendChild(img);
    node.insertBefore(layer, node.firstChild);
    bdSurfaces.push({ node: node, layer: layer, img: img });
  }

  // Offset of a node inside the window, from layout only (transforms are ignored).
  function layoutOffset(node) {
    let x = 0;
    let y = 0;
    for (let n = node; n && n.nodeType === 1; n = n.offsetParent) {
      x += n.offsetLeft;
      y += n.offsetTop;
    }
    return { x: x, y: y };
  }

  function syncBackdrops() {
    for (const s of bdSurfaces) {
      const p = layoutOffset(s.node);
      const mx = state.windowX + p.x;
      const my = state.windowY + p.y;
      s.layer.style.left = (-mx - BD_PAD) + 'px';
      s.layer.style.top = (-my - BD_PAD) + 'px';
    }
  }

  function paintBackdrop() {
    // Styles that cover the whole monitor also get the image under the margin.
    const edge = !!bd.image && (bd.style === 'fill' || bd.style === 'span' || bd.style === 'stretch');
    for (const s of bdSurfaces) {
      s.layer.style.width = (bd.mw + 2 * BD_PAD) + 'px';
      s.layer.style.height = (bd.mh + 2 * BD_PAD) + 'px';
      s.layer.style.backgroundColor = bd.color;
      s.layer.style.backgroundImage = edge ? 'url("' + bd.image + '")' : 'none';
      s.img.dataset.style = bd.style;
      s.img.style.backgroundImage = bd.image ? 'url("' + bd.image + '")' : 'none';
    }
    syncBackdrops();
  }

  function isPngDataUrl(v) {
    return typeof v === 'string' && /^data:image\/png;base64,[A-Za-z0-9+/]+=*$/.test(v);
  }

  // get_backdrop can fail. The panels then fall back to a solid #000 backdrop.
  function loadBackdrop() {
    const token = ++bd.token;
    return call('get_backdrop').then((data) => {
      if (token !== bd.token) return;
      const ok = !!data && typeof data === 'object' && Number(data.monitorWidth) > 0 && Number(data.monitorHeight) > 0;
      bd.mw = ok ? Number(data.monitorWidth) : (window.screen.width || window.innerWidth);
      bd.mh = ok ? Number(data.monitorHeight) : (window.screen.height || window.innerHeight);
      bd.image = ok && isPngDataUrl(data.image) ? data.image : null;
      bd.style = ok && BD_STYLES.indexOf(data.style) !== -1 ? data.style : 'fill';
      bd.color = ok && /^#[0-9a-fA-F]{6}$/.test(data.color) ? data.color : '#000000';
      paintBackdrop();
    });
  }

  // ---- Icons (get_icon, cached by launch + size; lazy by IntersectionObserver) ----

  const iconCache = new Map();

  function iconUrl(launch, size) {
    const key = size + '|' + launch;
    let pending = iconCache.get(key);
    if (!pending) {
      pending = call('get_icon', { launch: launch, size: size });
      iconCache.set(key, pending);
    }
    return pending;
  }

  const lazyIO = 'IntersectionObserver' in window
    ? new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        const target = entry.target;
        lazyIO.unobserve(target);
        const load = target._load;
        target._load = null;
        if (load) load();
      }
    })
    : null;

  function initialOf(name) {
    const text = typeof name === 'string' ? name.trim() : '';
    return text ? text.charAt(0).toUpperCase() : '?';
  }

  function loadIcon(wrap, launch, name, size) {
    wrap._launch = launch;
    wrap._fb.textContent = initialOf(name);
    iconUrl(launch, size).then((url) => {
      if (wrap._launch !== launch) return; // the element now shows another app
      if (typeof url === 'string' && url) {
        wrap._img.src = url;
        wrap._img.hidden = false;
        wrap._fb.hidden = true;
      } else {
        wrap._img.hidden = true;
        wrap._fb.hidden = false;
      }
    });
  }

  // An icon slot. With lazy, the load waits until the slot is on screen.
  function iconWrap(launch, name, size, lazy) {
    const wrap = h('span', { class: 'ico-wrap' });
    const img = h('img', { class: 'ico', alt: '', draggable: 'false', hidden: '' });
    const fallback = h('span', { class: 'ico-fb', 'aria-hidden': 'true' });
    wrap.append(img, fallback);
    wrap._img = img;
    wrap._fb = fallback;
    wrap._launch = null;
    fallback.textContent = initialOf(name);
    const load = () => loadIcon(wrap, launch, name, size);
    if (lazy && lazyIO) {
      wrap._load = load;
      lazyIO.observe(wrap);
    } else {
      load();
    }
    return wrap;
  }

  function appTile(app, size, cls) {
    return h('span', { class: 'tile ' + (cls || '') }, iconWrap(app.launch, app.name, size, true));
  }

  // ---- Start: pins, recent, all apps, user --------------------------------

  function isPinned(app) {
    return state.pins.some((p) => p.id === app.id);
  }

  function togglePin(app, wantPinned, after) {
    call('set_start_pin', {
      app: { id: app.id, name: app.name, launch: app.launch },
      pinned: wantPinned
    }).then((list) => {
      if (Array.isArray(list)) {
        state.pins = list;
        renderPins(list);
      }
      if (after) after();
    });
  }

  function launchApp(app) {
    call('launch', { launch: app.launch });
    closeFlyout();
  }

  function openFile(path) {
    call('open_path', { path: path });
    closeFlyout();
  }

  function appMenu(app, x, y) {
    const pinned = isPinned(app);
    showMenu([{ label: pinned ? 'Unpin from Start' : 'Pin to Start', run: () => togglePin(app, !pinned) }], x, y, false);
  }

  function renderPins(list) {
    el.pins.textContent = '';
    if (!list.length) {
      el.pins.appendChild(note('Right-click an app in All apps to pin it here.'));
      return;
    }
    for (const app of list) {
      const button = h('button', { class: 'pin', type: 'button' },
        appTile(app, ICON_PX, ''),
        h('span', { class: 'lbl ellipsis', text: app.name }));
      button.addEventListener('click', () => launchApp(app));
      button.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        appMenu(app, e.clientX, e.clientY);
      });
      el.pins.appendChild(button);
    }
  }

  // "Today, 9:32 PM", "Yesterday, 9:32 PM", "Tuesday, 12:38 PM", else "Nov 3, 9:32 PM".
  function formatWhen(ms) {
    if (typeof ms !== 'number' || !isFinite(ms) || ms <= 0) return '';
    const d = new Date(ms);
    const now = new Date();
    const time = d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
    const today = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const day = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
    const days = Math.round((today - day) / 86400000);
    if (days === 0) return 'Today, ' + time;
    if (days === 1) return 'Yesterday, ' + time;
    if (days > 1 && days < 7) return d.toLocaleDateString(undefined, { weekday: 'long' }) + ', ' + time;
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }) + ', ' + time;
  }

  function renderRecent(list) {
    el.recent.textContent = '';
    if (!list.length) {
      el.recent.appendChild(note('Nothing recent yet.'));
      return;
    }
    for (const file of list.slice(0, 4)) {
      const row = h('button', { class: 'row', type: 'button' },
        glyph('file'),
        h('span', { class: 'txt' },
          h('span', { class: 'row-name', text: file.name }),
          h('span', { class: 'sm ellipsis', text: formatWhen(file.modifiedMs) })));
      row.addEventListener('click', () => openFile(file.path));
      el.recent.appendChild(row);
    }
  }

  function categoryRank(name) {
    const i = CATEGORY_ORDER.indexOf(name);
    return i === -1 ? CATEGORY_ORDER.length : i;
  }

  function renderCategories(apps) {
    el.cats.textContent = '';
    state.groups = new Map();
    for (const app of apps) {
      if (!app || typeof app.name !== 'string') continue;
      const name = typeof app.category === 'string' && app.category ? app.category : 'Other';
      if (!state.groups.has(name)) state.groups.set(name, []);
      state.groups.get(name).push(app);
    }
    const names = Array.from(state.groups.keys()).sort((a, b) => categoryRank(a) - categoryRank(b) || a.localeCompare(b));
    for (const name of names) {
      const list = state.groups.get(name);
      const tile = h('span', { class: 'cat-tile' });
      for (const app of list.slice(0, 4)) {
        tile.appendChild(iconWrap(app.launch, app.name, ICON_PX, true));
      }
      const button = h('button', { class: 'cat', type: 'button' }, tile, h('span', { class: 'lbl', text: name }));
      button.addEventListener('click', () => openCategory(name));
      el.cats.appendChild(button);
    }
    if (state.groups.size === 0) {
      el.cats.appendChild(note('No apps found.'));
    }
  }

  function openCategory(name) {
    const list = state.groups.get(name) || [];
    el.catName.textContent = name;
    el.catList.textContent = '';
    for (const app of list) {
      const row = h('button', { class: 'row', type: 'button' }, appTile(app, ICON_PX, 'xs'), nameSpan(app.name));
      row.addEventListener('click', () => launchApp(app));
      row.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        appMenu(app, e.clientX, e.clientY);
      });
      el.catList.appendChild(row);
    }
    el.cats.hidden = true;
    el.catView.hidden = false;
    el.allScroll.scrollTop = 0;
  }

  function closeCategory() {
    el.catView.hidden = true;
    el.cats.hidden = false;
  }

  function renderUser(user) {
    const u = user && typeof user === 'object' ? user : {};
    el.userName.textContent = typeof u.name === 'string' ? u.name : '';
    const hasAvatar = typeof u.avatar === 'string' && u.avatar.length > 0;
    el.avatar.hidden = !hasAvatar;
    // The glyph is an SVG element: it has no `hidden` property, so set the attribute.
    if (hasAvatar) el.avatarGlyph.setAttribute('hidden', '');
    else el.avatarGlyph.removeAttribute('hidden');
    if (hasAvatar) el.avatar.src = u.avatar;
  }

  function refreshStart() {
    Promise.all([
      call('get_start_pins'),
      call('get_recent', { limit: 4 }),
      call('get_all_apps'),
      call('get_user')
    ]).then((results) => {
      if (Array.isArray(results[0])) state.pins = results[0];
      renderPins(state.pins);
      renderRecent(Array.isArray(results[1]) ? results[1] : []);
      if (Array.isArray(results[2])) state.apps = results[2];
      renderCategories(state.apps);
      renderUser(results[3]);
    });
  }

  // ---- Search: start (typing swaps in results) and the search panel --------

  function makeView(input, root, mode) {
    return { input: input, root: root, mode: mode, timer: 0, token: 0, rows: [], sel: -1, best: null, query: '' };
  }

  const startView = makeView(el.startQ, el.startResults, 'home');
  const searchView = makeView(el.searchQ, el.searchResults, 'hint');

  function setStartMode(showResults) {
    if (el.startHome.hidden === showResults) return; // no change
    el.startHome.hidden = showResults;
    el.startResults.hidden = !showResults;
    el.allScroll.scrollTop = 0;
    el.startResults.scrollTop = 0;
  }

  function clearResults(view) {
    view.token++;
    view.rows = [];
    view.sel = -1;
    view.best = null;
    view.query = '';
    view.root.textContent = '';
    if (view.mode === 'hint') {
      view.root.appendChild(note('Type to search apps, settings, and files.'));
    }
  }

  function onQueryInput(view) {
    const q = view.input.value.trim();
    clearTimeout(view.timer);
    view.timer = 0;
    if (view.mode === 'home') setStartMode(q.length > 0);
    if (!q) {
      clearResults(view);
      return;
    }
    view.timer = setTimeout(() => runSearch(view), SEARCH_DEBOUNCE_MS);
  }

  function runSearch(view) {
    view.timer = 0;
    const q = view.input.value.trim();
    if (!q) return;
    const token = ++view.token;
    call('search', { query: q }).then((data) => {
      if (token !== view.token) return; // a newer query or a clear won
      renderResults(view, q, data && typeof data === 'object' ? data : {});
    });
  }

  function pill(label, run) {
    const b = h('button', { class: 'pill-btn', type: 'button', text: label });
    b.addEventListener('click', run);
    return b;
  }

  function sectionLabel(text) {
    return h('div', { class: 'sec-label', text: text });
  }

  function addRow(view, parent, item) {
    const row = h('button', { class: 'row', type: 'button' });
    if (item.kind === 'app') {
      row.append(appTile(item.app, ICON_PX, 'xs'), nameSpan(item.app.name));
      row.addEventListener('contextmenu', (e) => {
        e.preventDefault();
        appMenu(item.app, e.clientX, e.clientY);
      });
    } else if (item.kind === 'file') {
      row.append(glyph('file'), nameSpan(item.file.name));
    } else {
      row.append(glyph('search'), nameSpan('Search the web for ' + view.query));
    }
    row.addEventListener('click', () => activate(view, item));
    view.rows.push({ item: item, el: row });
    parent.appendChild(row);
  }

  function renderResults(view, q, data) {
    const apps = (Array.isArray(data.apps) ? data.apps : []).slice(0, 6);
    const files = (Array.isArray(data.files) ? data.files : []).slice(0, 6);
    view.root.textContent = '';
    view.rows = [];
    view.sel = -1;
    view.query = q;

    const card = h('div', { class: 'res-best card' });
    if (apps.length) {
      const app = apps[0];
      view.best = { kind: 'app', app: app };
      const pinned = isPinned(app);
      card.append(
        h('span', { class: 'tile lg' }, iconWrap(app.launch, app.name, BIG_ICON_PX, false)),
        h('div', { class: 'res-name', text: app.name }),
        h('div', { class: 'sm', text: 'App · best match' }),
        h('div', { class: 'pill-col' },
          pill('Open', () => activate(view, view.best)),
          pill(pinned ? 'Unpin from Start' : 'Pin to Start', () => togglePin(app, !pinned, () => renderResults(view, q, data)))));
    } else if (files.length) {
      const file = files[0];
      view.best = { kind: 'file', file: file };
      card.append(
        h('span', { class: 'tile lg' }, glyph('file', 'g glyph-big')),
        h('div', { class: 'res-name', text: file.name }),
        h('div', { class: 'sm', text: 'File' }),
        h('div', { class: 'pill-col' }, pill('Open', () => activate(view, view.best))));
    } else {
      view.best = { kind: 'web' };
      card.append(
        h('span', { class: 'tile lg' }, glyph('search', 'g glyph-big')),
        h('div', { class: 'res-name', text: 'No matches' }),
        h('div', { class: 'sm', text: 'Try a web search' }),
        h('div', { class: 'pill-col' }, pill('Search the web', () => activate(view, { kind: 'web' }))));
    }

    const side = h('div', { class: 'res-side' });
    if (apps.length) {
      side.appendChild(sectionLabel('Apps'));
      apps.forEach((app) => addRow(view, side, { kind: 'app', app: app }));
    }
    if (files.length) {
      side.appendChild(sectionLabel('Files'));
      files.forEach((file) => addRow(view, side, { kind: 'file', file: file }));
    }
    side.appendChild(sectionLabel('Web'));
    addRow(view, side, { kind: 'web' });

    view.root.append(card, side);
  }

  function activate(view, item) {
    if (!item) return;
    if (item.kind === 'app') {
      launchApp(item.app);
    } else if (item.kind === 'file') {
      openFile(item.file.path);
    } else {
      const q = view.input.value.trim() || view.query;
      if (!q) return;
      call('web_search', { query: q });
      closeFlyout();
    }
  }

  function setSel(view, index) {
    if (view.sel >= 0 && view.rows[view.sel]) view.rows[view.sel].el.classList.remove('on');
    view.sel = index;
    if (index >= 0 && view.rows[index]) {
      const row = view.rows[index].el;
      row.classList.add('on');
      row.scrollIntoView({ block: 'nearest' });
    }
  }

  // Arrows move the selection through the rows. Enter launches the selected row,
  // or the best match when nothing is selected.
  function onQueryKey(view, e) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      if (!view.rows.length) return;
      e.preventDefault();
      const next = e.key === 'ArrowDown' ? view.sel + 1 : view.sel - 1;
      setSel(view, Math.max(0, Math.min(view.rows.length - 1, next)));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const picked = view.sel >= 0 && view.rows[view.sel] ? view.rows[view.sel].item : view.best;
      activate(view, picked);
    }
  }

  // ---- Quick settings ------------------------------------------------------

  function refreshSystem() {
    call('get_system').then(renderSystem);
  }

  function setTrack(track, value) {
    const v = Math.max(0, Math.min(1, value));
    track.dataset.value = String(v);
    const pct = (v * 100).toFixed(1) + '%';
    track.querySelector('.fill').style.width = pct;
    track.querySelector('.knob').style.left = pct;
    track.setAttribute('aria-valuenow', String(Math.round(v * 100)));
  }

  // Latest-value sender: one call in flight, the newest value goes next.
  function latestSender(cmd, key) {
    let busy = false;
    let pending = null;
    const pump = () => {
      if (pending === null) {
        busy = false;
        return;
      }
      const value = pending;
      pending = null;
      busy = true;
      const args = {};
      args[key] = value;
      call(cmd, args).then(pump);
    };
    return (value) => {
      pending = value;
      if (!busy) pump();
    };
  }

  const sendVolume = latestSender('set_volume', 'value');
  const sendBrightness = latestSender('set_brightness', 'value');

  function bindSlider(track, onChange) {
    const fromPointer = (clientX) => {
      const r = track.getBoundingClientRect();
      const v = r.width > 0 ? (clientX - r.left) / r.width : 0;
      setTrack(track, v);
      onChange(Math.max(0, Math.min(1, v)));
    };
    track.addEventListener('pointerdown', (e) => {
      if (e.button !== 0) return;
      track.setPointerCapture(e.pointerId);
      fromPointer(e.clientX);
    });
    track.addEventListener('pointermove', (e) => {
      if (track.hasPointerCapture(e.pointerId)) fromPointer(e.clientX);
    });
    track.addEventListener('keydown', (e) => {
      const cur = Number(track.dataset.value) || 0;
      let next = null;
      if (e.key === 'ArrowRight' || e.key === 'ArrowUp') next = cur + 0.05;
      else if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') next = cur - 0.05;
      else if (e.key === 'Home') next = 0;
      else if (e.key === 'End') next = 1;
      if (next === null) return;
      e.preventDefault();
      const v = Math.max(0, Math.min(1, next));
      setTrack(track, v);
      onChange(v);
    });
  }

  // A control is shown only when its data is present (null = unavailable here).
  function renderSystem(sys) {
    const s = sys && typeof sys === 'object' ? sys : {};
    state.system = s;

    const wifiKnown = typeof s.wifi === 'boolean';
    el.qsWifi.hidden = !wifiKnown;
    el.qsWifi.classList.toggle('act', s.wifi === true);
    el.qsWifi.setAttribute('aria-pressed', String(s.wifi === true));
    el.qsWifiLabel.textContent = typeof s.networkName === 'string' && s.networkName ? s.networkName : 'Wi-Fi';

    const btKnown = typeof s.bluetooth === 'boolean';
    el.qsBt.hidden = !btKnown;
    el.qsBt.classList.toggle('act', s.bluetooth === true);
    el.qsBt.setAttribute('aria-pressed', String(s.bluetooth === true));

    const brightKnown = typeof s.brightness === 'number';
    el.rowBright.hidden = !brightKnown;
    if (brightKnown) setTrack(el.bright, s.brightness);

    const volumeKnown = typeof s.volume === 'number';
    const muted = s.muted === true;
    el.rowVol.hidden = !volumeKnown;
    el.rowVol.classList.toggle('muted', muted);
    if (volumeKnown) setTrack(el.vol, s.volume);
    el.volBtn.textContent = '';
    el.volBtn.appendChild(glyph(muted ? 'mute' : 'volume'));
    el.volBtn.setAttribute('aria-label', muted ? 'Unmute' : 'Mute');

    const battery = s.battery && typeof s.battery === 'object' && typeof s.battery.percent === 'number' ? s.battery : null;
    el.qsStatus.hidden = !battery;
    if (battery) {
      let text = battery.percent + '%';
      if (battery.charging) text += ', charging';
      else if (s.pluggedIn === true) text += ', plugged in';
      el.qsStatusText.textContent = text;
    }
  }

  function toggleRadio(kind) {
    const on = kind === 'wifi' ? state.system.wifi === true : state.system.bluetooth === true;
    call('set_radio', { kind: kind, on: !on }).then(refreshSystem);
  }

  function openSettingsPage(page) {
    call('open_settings', { page: page });
    closeFlyout();
  }

  function sendShortcut(name) {
    call('send_shortcut', { name: name });
    closeFlyout();
  }

  // ---- Notifications: calendar ---------------------------------------------

  function resetCalendar() {
    const now = new Date();
    state.cal.year = now.getFullYear();
    state.cal.month = now.getMonth();
  }

  function shiftMonth(delta) {
    const d = new Date(state.cal.year, state.cal.month + delta, 1);
    state.cal.year = d.getFullYear();
    state.cal.month = d.getMonth();
    renderCalendar();
  }

  function renderCalendar() {
    const now = new Date();
    const y = state.cal.year;
    const m = state.cal.month;
    el.calToday.textContent = now.toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' });
    el.calMonth.textContent = new Date(y, m, 1).toLocaleDateString(undefined, { month: 'long', year: 'numeric' });
    el.cal.textContent = '';
    for (let i = 0; i < 7; i++) {
      // 1 Jan 2023 was a Sunday, so this runs Sunday to Saturday.
      el.cal.appendChild(h('span', { class: 'wk', text: new Date(2023, 0, 1 + i).toLocaleDateString(undefined, { weekday: 'narrow' }) }));
    }
    const first = new Date(y, m, 1).getDay();
    const days = new Date(y, m + 1, 0).getDate();
    for (let i = 0; i < first; i++) el.cal.appendChild(h('span', { class: 'd' }));
    for (let d = 1; d <= days; d++) {
      const isToday = d === now.getDate() && m === now.getMonth() && y === now.getFullYear();
      el.cal.appendChild(h('span', { class: isToday ? 'd today' : 'd', text: String(d) }));
    }
  }

  // ---- Menus ---------------------------------------------------------------

  function nodeFor(name) {
    return Object.prototype.hasOwnProperty.call(PANEL_IDS, name) ? $(PANEL_IDS[name]) : null;
  }

  // Confirm step: an item with `ask` swaps its label for the question on the
  // first click. The second click runs it. Escape, or the pointer leaving the
  // menu, puts the label back. Only one item can be armed at a time.
  let armedButton = null;

  function disarmMenu() {
    if (!armedButton) return false;
    armedButton.classList.remove('armed');
    armedButton.textContent = armedButton._label;
    armedButton = null;
    return true;
  }

  function armMenuItem(button) {
    disarmMenu();
    armedButton = button;
    button.classList.add('armed');
    button.textContent = button._ask;
  }

  // A small glass menu inside the open panel: at the pointer, or above an anchor.
  function showMenu(items, x, y, above) {
    const panel = state.panel ? nodeFor(state.panel) : null;
    if (!panel) return;
    el.menuItems.textContent = '';
    armedButton = null;
    for (const item of items) {
      const b = h('button', { class: 'menu-item', type: 'button', role: 'menuitem', text: item.label });
      b._label = item.label;
      b._ask = item.ask || null;
      b.addEventListener('click', () => {
        if (item.ask && armedButton !== b) {
          armMenuItem(b);
          return;
        }
        hideMenu();
        item.run();
      });
      el.menuItems.appendChild(b);
    }
    el.menu.hidden = false;
    const box = panel.getBoundingClientRect();
    const w = el.menu.offsetWidth;
    const hgt = el.menu.offsetHeight;
    const left = Math.max(box.left + 6, Math.min(x, box.right - w - 6));
    let top = above ? y - hgt - 6 : y;
    if (!above && top + hgt > box.bottom - 6) top = y - hgt;
    top = Math.max(box.top + 6, Math.min(top, box.bottom - hgt - 6));
    el.menu.style.left = left + 'px';
    el.menu.style.top = top + 'px';
    syncBackdrops();
    pushRegions();
  }

  function hideMenu() {
    if (el.menu.hidden) return;
    disarmMenu();
    el.menu.hidden = true;
    el.menuItems.textContent = '';
    pushRegions();
  }

  el.menu.addEventListener('pointerleave', () => disarmMenu());

  el.powerBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    const r = el.powerBtn.getBoundingClientRect();
    showMenu(POWER_ITEMS.map((spec) => ({
      label: spec.label,
      ask: spec.ask || null,
      run: () => {
        call('power', { action: spec.action });
        closeFlyout();
      }
    })), r.left, r.top, true);
  });

  // ---- Native hit regions (set_regions per frame while moving) ------------

  function pushRegions() {
    if (!state.panel) return;
    const node = nodeFor(state.panel);
    if (!node) return;
    const targets = node.dataset.multi ? Array.from(node.querySelectorAll('.glass')) : [node];
    if (!el.menu.hidden) targets.push(el.menu);
    const rects = [];
    for (const t of targets) {
      const r = t.getBoundingClientRect();
      if (r.width < 1 || r.height < 1) continue;
      // Scale the corner radius by the same factor as the spring.
      const k = t.offsetWidth ? r.width / t.offsetWidth : 1;
      const radius = parseFloat(getComputedStyle(t).borderTopLeftRadius) || 0;
      rects.push({ x: r.left, y: r.top, w: r.width, h: r.height, r: radius * k });
    }
    const key = JSON.stringify(rects);
    if (key === state.regionKey) return;
    state.regionKey = key;
    call('set_regions', { rects: rects });
  }

  function cancelAnim() {
    if (state.anim) cancelAnimationFrame(state.anim);
    state.anim = 0;
    clearTimeout(state.settleTimer);
    state.settleTimer = 0;
    if (state.endNode && state.endFn) state.endNode.removeEventListener('transitionend', state.endFn);
    state.endNode = null;
    state.endFn = null;
  }

  // Follows the panel every frame until its spring ends, then once more.
  function animate(node, token, onSettle) {
    cancelAnim();
    const prop = reduced() ? 'opacity' : 'transform';
    const finish = () => {
      if (token !== state.session) return;
      cancelAnim();
      pushRegions();
      if (onSettle) onSettle();
    };
    const frame = () => {
      if (token !== state.session) return;
      pushRegions();
      state.anim = requestAnimationFrame(frame);
    };
    const onEnd = (e) => {
      if (e.target === node && e.propertyName === prop) finish();
    };
    state.anim = requestAnimationFrame(frame);
    state.endNode = node;
    state.endFn = onEnd;
    node.addEventListener('transitionend', onEnd);
    state.settleTimer = setTimeout(finish, reduced() ? REDUCED_SETTLE_MS : SETTLE_MS);
  }

  function resetPanel(name) {
    if (name === 'start') {
      clearTimeout(startView.timer);
      el.startQ.value = '';
      clearResults(startView);
      setStartMode(false);
      closeCategory();
    } else if (name === 'search') {
      clearTimeout(searchView.timer);
      el.searchQ.value = '';
      clearResults(searchView);
    }
  }

  function snapClose(name) {
    const node = nodeFor(name);
    if (node) {
      node.classList.add('snap');
      node.classList.remove('open');
      void node.offsetWidth; // apply the closed state with no transition
      node.classList.remove('snap');
    }
    resetPanel(name);
  }

  // ---- Tray ------------------------------------------------------------------
  // One button per hosted icon, keyed by id so live updates swap only images.
  // Clicks go to the owning app through tray_click.

  const trayButtons = new Map(); // id -> button

  function trayButton(id) {
    const button = h('button', { class: 'tray-ic', type: 'button' });
    const img = h('img', { alt: '', draggable: 'false', hidden: '' });
    const fallback = h('span', { class: 'ico-fb', 'aria-hidden': 'true' });
    button.append(img, fallback);
    button._img = img;
    button._fb = fallback;
    // A double click also fires two clicks; only the first one is sent.
    button.addEventListener('click', (e) => {
      if (e.detail <= 1) call('tray_click', { id: id, button: 'left' });
    });
    button.addEventListener('dblclick', () => call('tray_click', { id: id, button: 'double' }));
    button.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      call('tray_click', { id: id, button: 'right' });
    });
    return button;
  }

  function renderTray(icons) {
    const list = Array.isArray(icons) ? icons : [];
    const seen = new Set();
    let index = 0;
    for (const icon of list) {
      if (!icon || typeof icon.id !== 'string' || seen.has(icon.id)) continue;
      seen.add(icon.id);
      let button = trayButtons.get(icon.id);
      if (!button) {
        button = trayButton(icon.id);
        trayButtons.set(icon.id, button);
      }
      const tip = typeof icon.tip === 'string' ? icon.tip : '';
      button.title = tip;
      button.setAttribute('aria-label', tip || 'Notification icon');
      const url = typeof icon.image === 'string' && icon.image.startsWith('data:image/png;base64,') ? icon.image : '';
      if (url) {
        if (button._img.getAttribute('src') !== url) button._img.src = url;
        button._img.hidden = false;
        button._fb.hidden = true;
      } else {
        button._img.hidden = true;
        button._fb.hidden = false;
        button._fb.textContent = initialOf(tip);
      }
      const at = el.trayGrid.children[index] || null;
      if (at !== button) el.trayGrid.insertBefore(button, at);
      index += 1;
    }
    for (const [id, button] of trayButtons) {
      if (!seen.has(id)) {
        button.remove();
        trayButtons.delete(id);
      }
    }
    el.trayEmpty.hidden = trayButtons.size > 0;
  }

  function refreshTray() {
    call('get_tray_icons').then((icons) => {
      if (icons) renderTray(icons);
    });
  }

  function prepare(name) {
    if (name === 'tray') refreshTray();
    else if (name === 'start') refreshStart();
    else if (name === 'quick_settings') refreshSystem();
    else if (name === 'notifications') {
      resetCalendar();
      renderCalendar();
    }
  }

  function focusFor(name) {
    if (name === 'start') el.startQ.focus({ preventScroll: true });
    else if (name === 'search') el.searchQ.focus({ preventScroll: true });
  }

  // ---- Show and close ------------------------------------------------------

  function onShow(payload) {
    const name = payload && typeof payload.panel === 'string' ? payload.panel : null;
    const node = name ? nodeFor(name) : null;
    if (!node) return;
    const token = ++state.session;
    cancelAnim();
    hideMenu();
    if (state.panel && state.panel !== name) snapClose(state.panel);
    state.panel = name;

    // The origin is the taskbar icon's centre, in window coordinates.
    const ox = Number(payload.originX);
    const oy = Number(payload.originY);
    const originX = isFinite(ox) ? ox - PAD : node.offsetWidth / 2;
    const originY = isFinite(oy) ? oy - PAD : node.offsetHeight;
    node.style.transformOrigin = originX + 'px ' + originY + 'px';

    state.windowX = Number(payload.windowX) || 0;
    state.windowY = Number(payload.windowY) || 0;
    if (name === 'tray') {
      // Sized by the dock from the icon count; resizes arrive as another flyout-show.
      const width = Number(payload.width);
      const height = Number(payload.height);
      if (isFinite(width) && width > 0) node.style.width = width + 'px';
      if (isFinite(height) && height > 0) node.style.height = height + 'px';
    }
    prepare(name);
    void node.offsetWidth; // start the spring from the closed state
    node.classList.add('open');
    syncBackdrops();
    state.regionKey = '';
    animate(node, token, null);
    focusFor(name);
  }

  function onClose() {
    const name = state.panel;
    const token = ++state.session;
    cancelAnim();
    hideMenu();
    if (!name) {
      call('flyout_hidden');
      return;
    }
    const node = nodeFor(name);
    if (node) node.classList.remove('open');
    animate(node, token, () => {
      state.panel = null;
      state.regionKey = '';
      resetPanel(name);
      call('flyout_hidden');
    });
  }

  // ---- Wiring --------------------------------------------------------------

  el.startQ.addEventListener('input', () => onQueryInput(startView));
  el.startQ.addEventListener('keydown', (e) => onQueryKey(startView, e));
  el.searchQ.addEventListener('input', () => onQueryInput(searchView));
  el.searchQ.addEventListener('keydown', (e) => onQueryKey(searchView, e));
  el.catBack.addEventListener('click', closeCategory);

  el.qsWifi.addEventListener('click', () => toggleRadio('wifi'));
  el.qsBt.addEventListener('click', () => toggleRadio('bluetooth'));
  el.qsFocus.addEventListener('click', () => openSettingsPage('focus'));
  el.qsNight.addEventListener('click', () => openSettingsPage('nightlight'));
  el.qsAir.addEventListener('click', () => openSettingsPage('airplane'));
  el.qsCast.addEventListener('click', () => sendShortcut('cast'));
  el.qsSettings.addEventListener('click', () => openSettingsPage('settings'));
  el.volBtn.addEventListener('click', () => {
    const muted = state.system.muted === true;
    call('set_muted', { muted: !muted }).then(refreshSystem);
  });
  bindSlider(el.bright, sendBrightness);
  bindSlider(el.vol, sendVolume);

  el.ntfOpen.addEventListener('click', () => sendShortcut('notifications'));
  el.calPrev.addEventListener('click', () => shiftMonth(-1));
  el.calNext.addEventListener('click', () => shiftMonth(1));

  // Escape cancels a pending confirm, then closes the menu, then the flyout.
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    e.preventDefault();
    if (!el.menu.hidden) {
      if (!disarmMenu()) hideMenu();
      return;
    }
    closeFlyout();
  });

  // Clicking anywhere else in the panel dismisses the menu.
  document.addEventListener('pointerdown', (e) => {
    if (!el.menu.hidden && !el.menu.contains(e.target)) hideMenu();
  }, true);

  // Right-click is for app menus only; inputs keep the default.
  document.addEventListener('contextmenu', (e) => {
    if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
  });

  window.addEventListener('resize', () => syncBackdrops());
  document.querySelectorAll('.glass').forEach(makeBackdropLayer);
  if (window.ResizeObserver) {
    const ro = new ResizeObserver(() => syncBackdrops());
    bdSurfaces.forEach((s) => ro.observe(s.node));
  }

  listen('backdrop-changed', () => loadBackdrop());
  listen('flyout-show', (event) => onShow(event ? event.payload : null));
  listen('flyout-close', () => onClose());
  listen('tray-changed', (event) => renderTray(event ? event.payload : null));

  renderPins(state.pins);
  refreshStart();
  refreshSystem();
  loadBackdrop();
})();
