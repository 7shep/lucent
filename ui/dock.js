// Lucent dock: the taskbar bar, one button per DockItem, the hover label, the
// start/search/quick settings/clock buttons, and the native hit regions.
// Plain IIFE, no build step. Text is always set with textContent.
(function () {
  'use strict';

  const tauri = window.__TAURI__;
  const ICON_PX = 32; // requested icon size; drawn at 24
  const LABEL_HIDE_MS = 120; // grace period so the pointer can cross into the label
  const PANELS = {
    start: { w: 440, h: 600, x: (vw) => (vw - 440) / 2 },
    search: { w: 440, h: 420, x: (vw) => (vw - 440) / 2 },
    quick_settings: { w: 300, h: 300, x: (vw) => vw - 10 - 300 },
    notifications: { w: 320, h: 480, x: (vw) => vw - 10 - 320 },
    tray: { w: 0, h: 0, x: () => 0 } // sized from the icon count in sizeTray
  };
  // Tray grid, in step with #p-tray in flyout.css.
  const TRAY = { cols: 4, cell: 36, gap: 4, pad: 10, emptyW: 176, emptyH: 56, edge: 10 };

  const el = {
    bar: document.getElementById('bar'),
    items: document.getElementById('items'),
    label: document.getElementById('label'),
    labelText: document.getElementById('label-text'),
    start: document.getElementById('b-start'),
    search: document.getElementById('b-search'),
    tray: document.getElementById('b-tray'),
    qs: document.getElementById('b-qs'),
    clock: document.getElementById('b-clock'),
    time: document.getElementById('time'),
    date: document.getElementById('date'),
    gWifi: document.getElementById('g-wifi'),
    gEth: document.getElementById('g-eth'),
    gVol: document.getElementById('g-vol'),
    gMute: document.getElementById('g-mute')
  };
  const panelButtons = {
    start: el.start,
    search: el.search,
    quick_settings: el.qs,
    notifications: el.clock,
    tray: el.tray
  };

  const state = {
    openPanel: null, // panel name from flyout-state, or null
    hover: null, // item record whose label is shown
    hideTimer: 0,
    clockTimer: 0,
    records: new Map(), // item id -> { id, el, icon, item, cycle, iconLaunch }
    regionKey: '',
    trayCount: -1 // icons in the open tray panel's layout
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

  // ---- Backdrop: the wallpaper behind each glass surface ------------------
  // Each .glass element gets a .bd layer (monitor size plus BD_PAD on every side,
  // blurred in CSS) holding a .bd-img that is exactly the monitor. Its offset is
  // minus the surface's monitor position. The dock window sits at
  // (0, monitorHeight - DOCK_H).

  const BD_PAD = 80; // keep in step with --bd-pad in glass.css
  const DOCK_H = 104;
  const BD_STYLES = ['fill', 'fit', 'stretch', 'tile', 'center', 'span'];
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

  // The label is centred on its left edge with translate(-50%), so its box sits
  // half its width to the left of its layout position once shown.
  function syncBackdrops() {
    const winY = bd.mh - DOCK_H;
    for (const s of bdSurfaces) {
      const p = layoutOffset(s.node);
      const mx = p.x - (s.node === el.label ? s.node.offsetWidth / 2 : 0);
      const my = winY + p.y;
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

  // get_backdrop can fail. The bar then falls back to a solid #000 backdrop.
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

  // ---- Icons (get_icon, cached by launch + size) --------------------------

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

  function initialOf(name) {
    const text = typeof name === 'string' ? name.trim() : '';
    return text ? text.charAt(0).toUpperCase() : '?';
  }

  function makeIcon() {
    const wrap = document.createElement('span');
    wrap.className = 'ico-wrap';
    const img = document.createElement('img');
    img.className = 'ico';
    img.alt = '';
    img.draggable = false;
    img.hidden = true;
    const fallback = document.createElement('span');
    fallback.className = 'ico-fb';
    fallback.setAttribute('aria-hidden', 'true');
    wrap.append(img, fallback);
    wrap._img = img;
    wrap._fb = fallback;
    wrap._launch = null;
    return wrap;
  }

  function loadIcon(wrap, launch, name) {
    wrap._launch = launch;
    wrap._fb.textContent = initialOf(name);
    iconUrl(launch, ICON_PX).then((url) => {
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

  // ---- Dock items ---------------------------------------------------------

  function createRecord(id) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'ic item';
    const icon = makeIcon();
    const dot = document.createElement('span');
    dot.className = 'dot';
    const flash = document.createElement('span');
    flash.className = 'flash';
    button.append(icon, dot, flash);

    const rec = { id: id, el: button, icon: icon, item: null, cycle: null, iconLaunch: null };
    button.addEventListener('click', () => onItemClick(rec));
    button.addEventListener('mousedown', (e) => {
      // Middle button: launch a new instance. Also keeps autoscroll off.
      if (e.button !== 1) return;
      e.preventDefault();
      launchItem(rec);
    });
    button.addEventListener('pointerenter', () => showLabel(rec));
    button.addEventListener('pointerleave', scheduleHide);
    return rec;
  }

  function updateRecord(rec, item) {
    rec.item = item;
    const wins = Array.isArray(item.windows) ? item.windows : [];
    const running = wins.length > 0;
    rec.el.classList.toggle('running', running);
    rec.el.classList.toggle('active', item.active === true);
    rec.el.classList.toggle('on', item.active === true);
    rec.el.classList.toggle('flashing', item.flashing === true);
    rec.el.setAttribute('aria-label', typeof item.name === 'string' ? item.name : 'App');
    rec.el.setAttribute('aria-pressed', String(item.active === true));
    if (!item.active) rec.cycle = null;
    if (rec.iconLaunch !== item.launch) {
      rec.iconLaunch = item.launch;
      loadIcon(rec.icon, item.launch, item.name);
    }
    if (state.hover === rec) {
      el.labelText.textContent = typeof item.name === 'string' ? item.name : '';
      placeLabel(rec);
    }
  }

  function renderItems(list) {
    const seen = new Set();
    const ordered = [];
    (Array.isArray(list) ? list : []).forEach((item) => {
      if (!item || typeof item.id !== 'string' || seen.has(item.id)) return;
      seen.add(item.id);
      let rec = state.records.get(item.id);
      if (!rec) {
        rec = createRecord(item.id);
        state.records.set(item.id, rec);
      }
      updateRecord(rec, item);
      ordered.push(rec);
    });
    for (const [id, rec] of state.records) {
      if (seen.has(id)) continue;
      if (state.hover === rec) hideLabelNow();
      rec.el.remove();
      state.records.delete(id);
    }
    ordered.forEach((rec) => el.items.appendChild(rec.el));
    updateRegions();
  }

  function launchItem(rec) {
    if (rec.item) call('launch', { launch: rec.item.launch });
  }

  // Not running: launch. One window: activate it (or minimise it if it is the
  // foreground window). Several: the topmost first; while the app is active,
  // each click moves to the next of its windows.
  function onItemClick(rec) {
    const item = rec.item;
    if (!item) return;
    const wins = Array.isArray(item.windows) ? item.windows : [];
    if (wins.length === 0) {
      call('launch', { launch: item.launch });
      return;
    }
    let index = 0;
    if (wins.length > 1 && item.active) {
      index = ((rec.cycle == null ? 0 : rec.cycle) + 1) % wins.length;
    }
    rec.cycle = index;
    call('activate_window', { hwnd: wins[index].hwnd });
  }

  // ---- Hover label --------------------------------------------------------

  function placeLabel(rec) {
    const r = rec.el.getBoundingClientRect();
    const centre = r.left + r.width / 2;
    const half = el.label.offsetWidth / 2;
    const vw = window.innerWidth;
    const x = Math.min(Math.max(centre, half + 12), vw - half - 12);
    el.label.style.left = x + 'px';
    syncBackdrops();
  }

  function showLabel(rec) {
    cancelHide();
    if (state.hover === rec) return;
    state.hover = rec;
    el.labelText.textContent = rec.item && typeof rec.item.name === 'string' ? rec.item.name : '';
    el.label.classList.add('show');
    placeLabel(rec);
    updateRegions();
  }

  function scheduleHide() {
    cancelHide();
    state.hideTimer = setTimeout(hideLabelNow, LABEL_HIDE_MS);
  }

  function cancelHide() {
    clearTimeout(state.hideTimer);
    state.hideTimer = 0;
  }

  function hideLabelNow() {
    cancelHide();
    if (!state.hover) return;
    state.hover = null;
    el.label.classList.remove('show');
    updateRegions();
  }

  // ---- Native hit regions -------------------------------------------------

  function rectOf(r, radius) {
    return { x: r.left, y: r.top, w: r.width, h: r.height, r: radius };
  }

  // The bar, plus the hovered item (padded for its lift and scale) and the
  // label while it is shown. Everything else passes clicks through.
  function updateRegions() {
    const rects = [rectOf(el.bar.getBoundingClientRect(), 22)];
    if (state.hover) {
      const r = state.hover.el.getBoundingClientRect();
      rects.push({ x: r.left - 8, y: r.top - 8, w: r.width + 16, h: r.height + 16, r: 14 });
      if (el.label.classList.contains('show')) {
        rects.push(rectOf(el.label.getBoundingClientRect(), 10));
      }
    }
    const key = JSON.stringify(rects);
    if (key === state.regionKey) return;
    state.regionKey = key;
    call('set_regions', { rects: rects });
  }

  // ---- Panels -------------------------------------------------------------

  // Tray panel size for `count` icons: up to TRAY.cols per row.
  function traySize(count) {
    if (count <= 0) return { w: TRAY.emptyW, h: TRAY.emptyH };
    const cols = Math.min(TRAY.cols, count);
    const rows = Math.ceil(count / TRAY.cols);
    return {
      w: TRAY.pad * 2 + cols * TRAY.cell + (cols - 1) * TRAY.gap,
      h: TRAY.pad * 2 + rows * TRAY.cell + (rows - 1) * TRAY.gap
    };
  }

  // Sizes the tray panel for `count` icons, centred on the chevron and kept on screen.
  function sizeTray(count) {
    const size = traySize(count);
    const r = el.tray.getBoundingClientRect();
    const centre = r.left + r.width / 2;
    PANELS.tray.w = size.w;
    PANELS.tray.h = size.h;
    PANELS.tray.x = (vw) => Math.max(TRAY.edge, Math.min(vw - TRAY.edge - size.w, centre - size.w / 2));
    state.trayCount = count;
  }

  // The chevron. Without the tray host (it failed to start), the native
  // Windows tray is shown instead.
  function openTray() {
    if (state.openPanel === 'tray') {
      call('flyout_request_close');
      return;
    }
    invoke('get_tray_icons').then(
      (icons) => {
        sizeTray(Array.isArray(icons) ? icons.length : 0);
        togglePanel('tray');
      },
      (err) => {
        console.warn('tray host unavailable', err);
        call('show_native_tray');
      }
    );
  }

  // Icons came or went while the tray panel is open: resize it in place.
  function onTrayChanged(icons) {
    if (state.openPanel !== 'tray') return;
    const count = Array.isArray(icons) ? icons.length : 0;
    const before = traySize(state.trayCount);
    const after = traySize(count);
    if (before.w === after.w && before.h === after.h) return;
    sizeTray(count);
    openPanel('tray');
  }

  function togglePanel(name) {
    if (state.openPanel === name) {
      call('flyout_request_close');
      return;
    }
    openPanel(name);
  }

  function openPanel(name) {
    const spec = Object.prototype.hasOwnProperty.call(PANELS, name) ? PANELS[name] : null;
    const button = panelButtons[name];
    if (!spec || !button) return;
    const r = button.getBoundingClientRect();
    call('flyout_open', {
      panel: name,
      x: spec.x(window.innerWidth),
      width: spec.w,
      height: spec.h,
      anchorX: r.left + r.width / 2
    });
  }

  function setLit() {
    for (const name of Object.keys(panelButtons)) {
      const on = state.openPanel === name;
      panelButtons[name].classList.toggle('on', on);
      panelButtons[name].setAttribute('aria-pressed', String(on));
    }
  }

  // ---- Quick settings glyphs (network, volume) ----------------------------

  function refreshSystem() {
    call('get_system').then(applySystem);
  }

  // SVG elements have no `hidden` property, so the attribute is set directly.
  function setHidden(node, hidden) {
    if (hidden) node.setAttribute('hidden', '');
    else node.removeAttribute('hidden');
  }

  // A glyph is shown only when its data exists. Failed reads show neither.
  function applySystem(sys) {
    const s = sys && typeof sys === 'object' ? sys : {};
    const wifiKnown = typeof s.wifi === 'boolean';
    setHidden(el.gEth, s.ethernet !== true);
    setHidden(el.gWifi, s.ethernet === true || !wifiKnown);
    const volumeKnown = typeof s.volume === 'number' || typeof s.muted === 'boolean';
    setHidden(el.gMute, !(volumeKnown && s.muted === true));
    setHidden(el.gVol, !volumeKnown || s.muted === true);
  }

  // ---- Clock (aligned to the minute) --------------------------------------

  function renderClock() {
    const now = new Date();
    el.time.textContent = now.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
    el.date.textContent = now.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }

  function msToNextMinute() {
    return 60000 - (Date.now() % 60000) + 30;
  }

  function scheduleClock() {
    clearTimeout(state.clockTimer);
    state.clockTimer = setTimeout(() => {
      renderClock();
      refreshSystem();
      scheduleClock();
    }, msToNextMinute());
  }

  // ---- Wiring -------------------------------------------------------------

  el.start.addEventListener('click', () => togglePanel('start'));
  el.search.addEventListener('click', () => togglePanel('search'));
  el.qs.addEventListener('click', () => togglePanel('quick_settings'));
  el.clock.addEventListener('click', () => togglePanel('notifications'));
  el.tray.addEventListener('click', openTray);

  el.label.addEventListener('pointerenter', cancelHide);
  el.label.addEventListener('pointerleave', scheduleHide);

  // The dock is a bar, not a page: no browser context menu.
  document.addEventListener('contextmenu', (e) => e.preventDefault());
  window.addEventListener('resize', () => {
    if (state.hover) placeLabel(state.hover);
    syncBackdrops();
    updateRegions();
  });

  document.querySelectorAll('.glass').forEach(makeBackdropLayer);
  if (window.ResizeObserver) {
    const ro = new ResizeObserver(() => syncBackdrops());
    bdSurfaces.forEach((s) => ro.observe(s.node));
  }

  listen('backdrop-changed', () => loadBackdrop());
  listen('dock-changed', (event) => renderItems(event.payload));
  listen('flyout-state', (event) => {
    const name = event && event.payload ? event.payload.panel : null;
    state.openPanel = typeof name === 'string' && Object.prototype.hasOwnProperty.call(PANELS, name) ? name : null;
    setLit();
    if (!state.openPanel) refreshSystem();
  });
  listen('start-key', () => togglePanel('start'));
  listen('tray-changed', (event) => onTrayChanged(event ? event.payload : null));

  renderClock();
  scheduleClock();
  refreshSystem();
  loadBackdrop();
  call('get_dock').then(renderItems);
  updateRegions();
})();
