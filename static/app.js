/* Add recipe: one entry point with a progressive-enhancement mode switch. */
(function () {
  var root = document.querySelector('[data-add-recipe]');
  if (!root) return;
  var tabs = root.querySelectorAll('[data-add-mode]');
  var panels = root.querySelectorAll('[data-add-panel]');

  function setMode(mode) {
    var index;
    for (index = 0; index < tabs.length; index += 1) {
      var active = tabs[index].getAttribute('data-add-mode') === mode;
      tabs[index].className = 'add-mode-tab' + (active ? ' is-active' : '');
      tabs[index].setAttribute('aria-selected', active ? 'true' : 'false');
    }
    for (index = 0; index < panels.length; index += 1) {
      panels[index].hidden = panels[index].getAttribute('data-add-panel') !== mode;
    }
  }

  for (var i = 0; i < tabs.length; i += 1) {
    tabs[i].addEventListener('click', function (event) {
      event.preventDefault();
      setMode(this.getAttribute('data-add-mode'));
      if (window.history && window.history.replaceState) window.history.replaceState({}, '', this.href);
    });
  }
}());

/* Recipe page: the chart swaps in place, the cooking-step timer counts down
 * and announces its end once, an ingredient or step link opens its edit form
 * without a reload, and a pending dish-photo check refreshes only its own
 * panel, and only while no edit form or field is in use. */
(function () {
  var CHART_LINKS = '[data-recipe-view], [data-chart-step], [data-chart-start], [data-chart-back], [data-chart-next], [data-chart-exit]';
  var THUMB_INTERVAL = 3000;
  var THUMB_LIMIT = 100;
  var timerId = null;
  var thumbTimer = null;
  var thumbBusy = false;
  var thumbPolls = 0;

  function format(seconds) {
    var minutes = Math.floor(seconds / 60);
    var remainder = seconds % 60;
    return minutes + ':' + (remainder < 10 ? '0' : '') + remainder;
  }
  function stopTimer() {
    if (timerId) { window.clearInterval(timerId); timerId = null; }
  }
  function startTimer() {
    stopTimer();
    var detail = document.querySelector('[data-chart-detail]');
    var seconds = detail ? parseInt(detail.getAttribute('data-timer-seconds'), 10) || 0 : 0;
    var output = detail && detail.querySelector('[data-timer-value]');
    if (!seconds || !output) return;
    var deadline = new Date().getTime() + seconds * 1000;
    function tick() {
      var remaining = Math.max(0, Math.ceil((deadline - new Date().getTime()) / 1000));
      output.textContent = format(remaining);
      if (!remaining) {
        stopTimer();
        var note = detail.querySelector('[data-timer-announce]');
        if (note) note.textContent = "Time's up";
      }
    }
    tick();
    timerId = window.setInterval(tick, 250);
  }
  function thumbPending() {
    return document.querySelector('[data-thumb-pending]');
  }
  function editingNow() {
    var active = document.activeElement;
    if (active && /^(input|textarea|select)$/i.test(active.tagName)) return true;
    var forms = document.querySelectorAll('.edit-form');
    for (var index = 0; index < forms.length; index += 1) {
      if (!forms[index].hidden) return true;
    }
    return !!document.querySelector('.recipe-details-form');
  }
  function swapThumbs(markup) {
    var holder = document.createElement('div');
    holder.innerHTML = markup;
    var fresh = holder.querySelector ? holder.querySelector('.thumb-manage') : null;
    var current = document.querySelector('.thumb-manage');
    if (!fresh || !current || !current.parentNode) return;
    var before = current.getBoundingClientRect();
    var offset = window.pageYOffset || 0;
    current.parentNode.replaceChild(fresh, current);
    if (before.bottom <= 0 && window.scrollTo) {
      window.scrollTo(0, offset + fresh.getBoundingClientRect().bottom - before.bottom);
    }
  }
  function scheduleThumb() {
    if (thumbTimer || thumbBusy || !thumbPending()) return;
    thumbTimer = window.setTimeout(thumbTick, THUMB_INTERVAL);
  }
  function thumbTick() {
    thumbTimer = null;
    var pending = thumbPending();
    if (!pending || thumbBusy) return;
    if (thumbPolls >= THUMB_LIMIT) return;
    if (editingNow()) { scheduleThumb(); return; }
    thumbPolls += 1;
    thumbBusy = true;
    var request = new XMLHttpRequest();
    request.open('GET', '/recipes/' + pending.getAttribute('data-thumb-pending'), true);
    request.onreadystatechange = function () {
      if (request.readyState !== 4) return;
      thumbBusy = false;
      if (request.status >= 200 && request.status < 300) swapThumbs(request.responseText);
      scheduleThumb();
    };
    request.send();
  }
  function chartReady() {
    startTimer();
    scheduleThumb();
    var selected = document.querySelector('.chart-cell.is-active');
    if (selected && selected.scrollIntoView) selected.scrollIntoView(false);
  }
  function replacePaper(markup, url) {
    var holder = document.createElement('div');
    holder.innerHTML = markup;
    var next = holder.querySelector ? holder.querySelector('.paper') : null;
    var paper = document.querySelector('.paper');
    if (!next || !paper) { window.location.href = url; return; }
    stopTimer();
    paper.innerHTML = next.innerHTML;
    if (window.history && window.history.replaceState) window.history.replaceState({}, '', url);
    chartReady();
  }
  function navigate(url) {
    var request = new XMLHttpRequest();
    request.open('GET', url, true);
    request.onreadystatechange = function () {
      if (request.readyState !== 4) return;
      if (request.status >= 200 && request.status < 300) replacePaper(request.responseText, url);
      else window.location.href = url;
    };
    request.send();
  }

  document.addEventListener('click', function (event) {
    var target = event.target;
    var chartLink = target.closest ? target.closest(CHART_LINKS) : null;
    if (chartLink) { event.preventDefault(); navigate(chartLink.href); return; }
    var editLink = target.closest ? target.closest('[data-edit-block]') : null;
    if (!editLink) return;
    var form = document.getElementById('edit-' + editLink.getAttribute('data-edit-block'));
    if (!form) return;
    event.preventDefault();
    form.hidden = false;
    if (form.scrollIntoView) form.scrollIntoView(true);
    var field = form.querySelector('input,textarea');
    if (field) field.focus();
  });
  document.addEventListener('keydown', function (event) {
    if (event.altKey || event.ctrlKey || event.metaKey || (event.target && /input|textarea|select/i.test(event.target.tagName))) return;
    var link = event.keyCode === 37 ? document.querySelector('[data-chart-back]')
      : event.keyCode === 39 ? document.querySelector('[data-chart-next]') : null;
    if (link) { event.preventDefault(); navigate(link.href); }
  });
  chartReady();
}());

/* Global behaviour: destructive controls ask first, and a submitting form's
 * primary button shows its busy label and is disabled so a double tap cannot
 * repeat the request. Pages restored from the back-forward cache get their
 * buttons back. Without JavaScript the forms still submit normally. */
(function () {
  var busy = [];

  function ancestorWithAttribute(node, name) {
    while (node && node.nodeType === 1) {
      if (node.hasAttribute && node.hasAttribute(name)) return node;
      node = node.parentNode;
    }
    return null;
  }
  function markBusy(form, label) {
    var button = form.querySelector('button.primary') || form.querySelector('button');
    if (!button) return;
    busy.push({ button: button, text: button.textContent });
    button.textContent = label;
    button.disabled = true;
  }
  function resetBusy() {
    while (busy.length) {
      var entry = busy.pop();
      entry.button.disabled = false;
      entry.button.textContent = entry.text;
    }
  }

  document.addEventListener('click', function (event) {
    var node = ancestorWithAttribute(event.target, 'data-confirm');
    if (!node || node.nodeName === 'FORM') return;
    if (!window.confirm(node.getAttribute('data-confirm'))) event.preventDefault();
  });
  document.addEventListener('submit', function (event) {
    var form = event.target;
    if (!form || form.nodeName !== 'FORM') return;
    var message = form.getAttribute('data-confirm');
    if (message && !window.confirm(message)) {
      event.preventDefault();
      return;
    }
    var label = form.getAttribute('data-busy-label');
    if (label) markBusy(form, label);
  });
  window.addEventListener('pageshow', resetBusy);
}());

/* From Video import: replay the bounded run feed, then poll for new events.
 * The background job survives navigation and each raw channel is replaced
 * atomically when its backend stage reports, avoiding partial or unsafe HTML. */
(function () {
  var root = document.querySelector('[data-import-root]');
  if (!root) return;
  var READING = 'Reading the video…';
  var CLEANING = 'Cleaning the video evidence…';
  var PREPARING = 'Preparing the recipe draft…';
  var DRAFTING = 'Building the recipe draft…';
  var READY = 'Recipe draft ready.';
  var OCR_COPY = 'Reading on-screen text…';
  var MAX_FAILURES = 4;
  var RETRY_MS = 3500;
  var finished = root.getAttribute('data-import-finished') === '1';
  var eventsUrl = root.getAttribute('data-events-url');
  var framesBase = root.getAttribute('data-frames-base');
  var status = root.querySelector('[data-import-status]');
  var errorBox = root.querySelector('[data-import-error]');
  var warnings = root.querySelector('[data-import-warnings]');
  var ready = root.querySelector('[data-import-ready]');
  var draftLink = root.querySelector('[data-import-draft]');
  var draftCopy = root.querySelector('[data-import-draft-copy]');
  var progressBar = root.querySelector('[data-import-progressbar]');
  var progressCopy = root.querySelector('[data-import-progress-copy]');
  var progressNames = ['extract', 'clean', 'generate'];
  var progress = { extract: 0, clean: 0, generate: 0 };
  var parts = {};
  var stages = {};
  var since = 0;
  var failures = 0;
  var stopped = false;
  var replayed = false;

  for (var p = 0; p < progressNames.length; p += 1) {
    var name = progressNames[p];
    parts[name] = {
      segment: root.querySelector('[data-progress-segment="' + name + '"]'),
      fill: root.querySelector('[data-progress-fill="' + name + '"]'),
      badge: root.querySelector('[data-progress-status="' + name + '"]')
    };
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }
  function clear(node) { while (node && node.firstChild) node.removeChild(node.firstChild); }
  function clamp(value) { return Math.max(0, Math.min(1, value)); }
  function progressState(value) { return value >= 1 ? 'done' : value > 0 ? 'working' : 'waiting'; }
  function drawProgress() {
    var total = 0;
    var index;
    for (index = 0; index < progressNames.length; index += 1) {
      var name = progressNames[index];
      var value = clamp(progress[name]);
      var part = parts[name];
      if (part.fill) part.fill.style.width = Math.round(value * 100) + '%';
      if (part.segment) part.segment.className = 'import-progress-segment' + (value >= 1 ? ' is-complete' : value > 0 ? ' is-active' : '');
      if (part.badge) part.badge.textContent = progressState(value);
      total += value;
    }
    total = Math.round(total / progressNames.length * 100);
    if (progressBar) {
      progressBar.setAttribute('aria-valuenow', String(total));
      progressBar.setAttribute('aria-valuetext', total + '% complete');
    }
  }
  function advanceProgress(name, value, copy) {
    progress[name] = Math.max(progress[name], clamp(value));
    if (copy && progressCopy) progressCopy.textContent = copy;
    drawProgress();
  }
  function initialiseProgress() {
    var state = (root.getAttribute('data-import-progress-state') || '').toLowerCase();
    if (root.getAttribute('data-import-progress-complete') === '1') {
      progress.extract = 1;
      progress.clean = 1;
      progress.generate = 1;
      if (progressCopy) progressCopy.textContent = READY;
    } else if (state.indexOf('preparing') !== -1 || state.indexOf('draft') !== -1) {
      progress.extract = 1;
      progress.clean = 1;
      progress.generate = .08;
      if (progressCopy) progressCopy.textContent = DRAFTING;
    } else if (state.indexOf('clean') !== -1) {
      progress.extract = 1;
      progress.clean = .08;
      if (progressCopy) progressCopy.textContent = CLEANING;
    } else {
      progress.extract = .03;
      if (progressCopy) progressCopy.textContent = READING;
    }
    drawProgress();
  }
  function cleaning() {
    advanceProgress('extract', 1, CLEANING);
    advanceProgress('clean', .08, CLEANING);
  }
  function drafting() {
    advanceProgress('extract', 1, DRAFTING);
    advanceProgress('clean', 1, DRAFTING);
    advanceProgress('generate', .08, DRAFTING);
  }
  function applyProgress(event) {
    var state = (event.state || '').toLowerCase();
    if (event.kind === 'status') {
      if (state.indexOf('clean') !== -1) cleaning();
      else if (state.indexOf('preparing') !== -1 || state.indexOf('draft') !== -1) drafting();
      else if (state.indexOf('extract') !== -1 || state.indexOf('wait') !== -1) advanceProgress('extract', .06, READING);
    } else if (event.kind === 'phase') {
      if (event.phase === 'worker') advanceProgress('extract', state.indexOf('wait') !== -1 ? .03 : .06, READING);
      else if (event.phase === 'description') advanceProgress('extract', state === 'done' ? .25 : .12, READING);
      else if (event.phase === 'download') advanceProgress('extract', state === 'done' ? .43 : .34, 'Downloading the video…');
      else if (event.phase === 'audio') advanceProgress('extract', state === 'done' ? .68 : .54, 'Transcribing spoken audio…');
      else if (event.phase === 'ocr') advanceProgress('extract', state === 'done' ? .96 : state.indexOf('sampling') !== -1 ? .71 : .79, OCR_COPY);
      else if (event.phase === 'cleaner') cleaning();
      else if (event.phase === 'draft') drafting();
    } else if (event.kind === 'ocr-plan') {
      advanceProgress('extract', .82, OCR_COPY);
    } else if (event.kind === 'ocr-captures') {
      advanceProgress('extract', .97, OCR_COPY);
    } else if (event.kind === 'cleaned') {
      advanceProgress('extract', 1, PREPARING);
      advanceProgress('clean', 1, PREPARING);
    } else if (event.kind === 'result') {
      advanceProgress('extract', 1, READY);
      advanceProgress('clean', 1, READY);
      advanceProgress('generate', 1, READY);
    } else if (event.kind === 'error' && progressCopy) {
      progressCopy.textContent = 'Import stopped.';
    }
  }
  function stage(name) {
    if (!stages[name]) {
      var box = root.querySelector('[data-import-stage="' + name + '"]');
      stages[name] = {
        enabled: !box || box.getAttribute('data-stage-enabled') !== '0',
        badge: box ? box.querySelector('[data-stage-status]') : null
      };
    }
    return stages[name];
  }
  function stageStatus(name, value) {
    var entry = stage(name);
    if (entry.badge && entry.enabled) entry.badge.textContent = value;
  }
  function setRaw(holder, text, emptyText) {
    if (!holder) return;
    clear(holder);
    holder.appendChild(text ? el('div', 'debug-mono', text) : el('p', 'muted', emptyText));
  }
  function setDescription(event) {
    if (!stage('description').enabled) return;
    var holder = root.querySelector('[data-import-description]');
    if (!holder) return;
    clear(holder);
    if (event.durationSeconds !== undefined && event.durationSeconds !== null) holder.appendChild(el('p', 'muted', 'Duration: ' + event.durationSeconds + 's'));
    if (event.title) holder.appendChild(el('p', null, '')).appendChild(el('strong', null, event.title));
    if (event.description) holder.appendChild(el('div', 'debug-mono', event.description));
    if (!event.title && !event.description) holder.appendChild(el('p', 'muted', 'No caption or description was found.'));
    stageStatus('description', 'done');
  }
  function setAudio(event) {
    if (!stage('audio').enabled) return;
    setRaw(root.querySelector('[data-import-audio]'), event.transcript || '', 'No spoken transcript was found.');
    stageStatus('audio', 'done');
  }
  function setOcr(event) {
    if (!stage('ocr').enabled) return;
    var holder = root.querySelector('[data-import-ocr]');
    if (!holder) return;
    var captures = event.captures || [];
    clear(holder);
    if (!captures.length) {
      holder.appendChild(el('p', 'muted', 'No readable on-screen text was found.'));
      stageStatus('ocr', 'done');
      return;
    }
    var table = el('table', 'debug-captures');
    var head = table.createTHead().insertRow();
    ['Frame', 't', 'Recognised text', 'Raw engine reading'].forEach(function (label) {
      head.appendChild(document.createElement('th')).appendChild(document.createTextNode(label));
    });
    var body = table.appendChild(document.createElement('tbody'));
    captures.forEach(function (capture) {
      var row = body.insertRow(-1);
      var thumb = row.insertCell(-1);
      thumb.className = 'debug-thumb';
      if (capture.image) {
        var link = el('a');
        var image = el('img');
        link.href = framesBase + '/0/' + capture.image;
        link.target = '_blank';
        image.src = link.href;
        image.alt = 'frame at ' + capture.seconds + 's';
        image.loading = 'lazy';
        link.appendChild(image);
        thumb.appendChild(link);
      } else {
        thumb.textContent = '—';
      }
      row.insertCell(-1).textContent = capture.seconds + 's';
      var clean = row.insertCell(-1);
      if (capture.text !== undefined && capture.text !== null) clean.textContent = capture.text;
      else clean.appendChild(el('span', 'debug-dropped', 'not kept'));
      var raw = row.insertCell(-1);
      raw.className = 'debug-mono debug-raw';
      raw.textContent = capture.raw || '';
    });
    var scroll = el('div', 'table-scroll');
    scroll.appendChild(table);
    holder.appendChild(scroll);
    stageStatus('ocr', 'done');
  }
  function apply(event) {
    applyProgress(event);
    if (event.kind === 'status') {
      if (status) status.textContent = event.state || 'working';
    } else if (event.kind === 'phase') {
      stageStatus(event.phase, event.state || 'working');
    } else if (event.kind === 'description') {
      setDescription(event);
    } else if (event.kind === 'audio') {
      setAudio(event);
    } else if (event.kind === 'ocr-plan') {
      stageStatus('ocr', 'reading ' + event.planned + ' frames');
    } else if (event.kind === 'ocr-captures') {
      setOcr(event);
    } else if (event.kind === 'cleaned') {
      setRaw(root.querySelector('[data-import-cleaner]'), event.text || '', 'The cleaner returned no recipe facts.');
      stageStatus('cleaner', 'done');
    } else if (event.kind === 'warning' && event.message && warnings) {
      warnings.appendChild(el('li', null, event.message));
    } else if (event.kind === 'result') {
      if (status) {
        status.textContent = 'ready';
        status.className = 'codex-status codex-status--connected import-overall-status';
      }
      stageStatus('draft', 'done');
      if (event.draftUrl && draftLink) draftLink.href = event.draftUrl;
      if (ready) ready.hidden = false;
      if (draftCopy) draftCopy.textContent = 'Ready for your review.';
    } else if (event.kind === 'error') {
      if (status) {
        status.textContent = 'failed';
        status.className = 'codex-status codex-status--missing import-overall-status';
      }
      if (errorBox) {
        errorBox.textContent = event.message || 'The video import failed.';
        errorBox.hidden = false;
      }
    }
  }
  function giveUp() {
    stopped = true;
    if (progressCopy) progressCopy.textContent = 'Import stopped.';
    if (status) {
      status.textContent = 'unavailable';
      status.className = 'codex-status codex-status--missing import-overall-status';
    }
    if (errorBox) {
      clear(errorBox);
      errorBox.appendChild(document.createTextNode('We lost touch with this import, so it may have expired. '));
      var link = el('a', null, 'Import another video');
      link.href = '/recipes/new?mode=video';
      errorBox.appendChild(link);
      errorBox.hidden = false;
    }
  }
  function retry() {
    failures += 1;
    if (failures >= MAX_FAILURES) { giveUp(); return; }
    window.setTimeout(poll, RETRY_MS);
  }
  function poll() {
    if (stopped) return;
    var request = new XMLHttpRequest();
    request.open('GET', eventsUrl + '?since=' + since, true);
    request.onreadystatechange = function () {
      if (request.readyState !== 4) return;
      var payload = null;
      if (request.status >= 200 && request.status < 300) {
        try { payload = JSON.parse(request.responseText); } catch (ignored) { payload = null; }
      }
      if (!payload) { retry(); return; }
      failures = 0;
      if (!replayed) { clear(warnings); replayed = true; }
      try { (payload.events || []).forEach(apply); } catch (ignored) { /* keep polling */ }
      since = payload.next || since;
      if (payload.done) { stopped = true; return; }
      window.setTimeout(poll, 900);
    };
    request.send();
  }
  initialiseProgress();
  if (!finished) poll();
}());
