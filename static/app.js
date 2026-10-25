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
  var momentsTimer = null;
  var momentsPolls = 0;

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
  function momentsPending() {
    return document.querySelector('[data-moments-pending]');
  }
  function scheduleMoments() {
    if (momentsTimer || !momentsPending()) return;
    momentsTimer = window.setTimeout(momentsTick, THUMB_INTERVAL);
  }
  // A finished capture changes every step, so the page reloads once the
  // fetched copy no longer shows the pending note.
  function momentsTick() {
    momentsTimer = null;
    var pending = momentsPending();
    if (!pending || momentsPolls >= THUMB_LIMIT) return;
    if (editingNow()) { scheduleMoments(); return; }
    momentsPolls += 1;
    var request = new XMLHttpRequest();
    request.open('GET', '/recipes/' + pending.getAttribute('data-moments-pending'), true);
    request.onreadystatechange = function () {
      if (request.readyState !== 4) return;
      if (request.status >= 200 && request.status < 300 && request.responseText.indexOf('data-moments-pending') === -1) {
        window.location.reload();
        return;
      }
      scheduleMoments();
    };
    request.send();
  }
  function chartReady() {
    startTimer();
    scheduleThumb();
    scheduleMoments();
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
 * atomically when its backend stage reports, avoiding partial or unsafe HTML.
 * The plain summary words each source like `import_sources` in ai.rs. The
 * technical details remember whether they were left open, and while they
 * are closed a finished import opens its draft by itself. */
(function () {
  var root = document.querySelector('[data-import-root]');
  if (!root) return;
  var READING = 'Watching the video…';
  var CLEANING = 'Picking out the recipe…';
  var PREPARING = 'Writing the draft…';
  var DRAFTING = 'Writing the draft…';
  var READY = 'Done.';
  var DETAILS_KEY = 'recipe-import-details';
  var OPEN_DELAY_MS = 1200;
  var OCR_COPY = 'Reading the text on screen…';
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
  var heading = root.querySelector('[data-import-heading]');
  var retryBox = root.querySelector('[data-import-retry]');
  var readyCopy = root.querySelector('[data-import-ready-copy]');
  var details = root.querySelector('[data-import-details]');
  var notesCount = root.querySelector('[data-import-notes-count]');
  var progressBar = root.querySelector('[data-import-progressbar]');
  var progressCopy = root.querySelector('[data-import-progress-copy]');
  var progressNames = ['extract', 'clean', 'generate'];
  var progress = { extract: 0, clean: 0, generate: 0 };
  // Audio transcription and on-screen text run side by side, so the copy
  // names whichever of the two is still going.
  var lanesDone = { audio: false, ocr: false };
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
      else if (event.phase === 'audio') {
        if (state === 'done') lanesDone.audio = true;
        advanceProgress('extract', state === 'done' ? .68 : .54, lanesCopy());
      } else if (event.phase === 'ocr') {
        if (state === 'done') lanesDone.ocr = true;
        advanceProgress('extract', state === 'done' ? .96 : state.indexOf('sampling') !== -1 ? .71 : .79, lanesCopy());
      }
      // Dish photos and video moments are computed in the background while
      // the cleaner and recipe draft run, so they never move the bar or
      // replace the copy of the stage that is actually running.
      else if (event.phase === 'cleaner') cleaning();
      else if (event.phase === 'draft') drafting();
    } else if (event.kind === 'ocr-plan') {
      advanceProgress('extract', .82, lanesCopy());
    } else if (event.kind === 'ocr-captures') {
      advanceProgress('extract', .97, lanesCopy());
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
  function lanesCopy() {
    var audioPending = stage('audio').enabled && !lanesDone.audio;
    var ocrPending = stage('ocr').enabled && !lanesDone.ocr;
    if (audioPending && ocrPending) return 'Listening and reading the text on screen…';
    if (audioPending) return 'Listening to the video…';
    if (ocrPending) return OCR_COPY;
    return READING;
  }
  function count(n, one, many) { return n + ' ' + (n === 1 ? one : many); }
  function words(text) {
    var trimmed = (text || '').replace(/^\s+|\s+$/g, '');
    return trimmed ? trimmed.split(/\s+/).length : 0;
  }
  function setFound(key, state, detail) {
    var item = root.querySelector('[data-found="' + key + '"]');
    if (!item || item.className === 'is-skipped') return;
    item.className = 'is-' + state;
    var holder = item.querySelector('[data-found-detail]');
    if (holder) holder.textContent = detail;
  }
  function foundDescription(event) {
    var n = words(event.description);
    if (n) setFound('description', 'found', count(n, 'word', 'words'));
    else if (event.title) setFound('description', 'found', 'Title only');
    else setFound('description', 'none', 'No caption');
  }
  function foundAudio(event) {
    var n = words(event.transcript);
    if (n) setFound('audio', 'found', count(n, 'word', 'words'));
    else setFound('audio', 'none', 'No speech');
  }
  function foundOcr(event) {
    var cards = event.cards || [];
    var captures = event.captures || [];
    var n = 0;
    var index;
    for (index = 0; index < cards.length; index += 1) if (cards[index].kept) n += 1;
    if (!n) {
      for (index = 0; index < captures.length; index += 1) {
        if (captures[index].text !== undefined && captures[index].text !== null) n += 1;
      }
    }
    if (n) setFound('ocr', 'found', count(n, 'card', 'cards'));
    else setFound('ocr', 'none', 'No text found');
  }
  function unreadFound() {
    var items = root.querySelectorAll('[data-found]');
    for (var index = 0; index < items.length; index += 1) {
      if (items[index].className === 'is-waiting') setFound(items[index].getAttribute('data-found'), 'none', 'Not read');
    }
  }
  function countNotes() {
    if (!notesCount || !warnings) return;
    var n = warnings.getElementsByTagName('li').length;
    notesCount.textContent = n ? '\u00b7 ' + count(n, 'note', 'notes') : '';
  }
  function rememberDetails() {
    try { window.localStorage.setItem(DETAILS_KEY, details.open ? '1' : '0'); } catch (ignored) { /* private mode */ }
  }
  function restoreDetails() {
    if (!details) return;
    try { if (window.localStorage.getItem(DETAILS_KEY) === '1') details.open = true; } catch (ignored) { /* private mode */ }
    var summary = details.getElementsByTagName('summary')[0];
    if (summary) summary.addEventListener('click', function () { window.setTimeout(rememberDetails, 0); });
  }
  function openDraft(url) {
    if (!url || (details && details.open)) {
      if (readyCopy) readyCopy.textContent = '';
      return;
    }
    if (readyCopy) readyCopy.textContent = 'Opening it now…';
    window.setTimeout(function () {
      if (!details || !details.open) window.location.href = url;
      else if (readyCopy) readyCopy.textContent = '';
    }, OPEN_DELAY_MS);
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
      if (stage('description').enabled) foundDescription(event);
      setDescription(event);
    } else if (event.kind === 'audio') {
      if (stage('audio').enabled) foundAudio(event);
      setAudio(event);
    } else if (event.kind === 'ocr-plan') {
      stageStatus('ocr', 'reading ' + event.planned + ' frames');
    } else if (event.kind === 'ocr-captures') {
      if (stage('ocr').enabled) foundOcr(event);
      setOcr(event);
    } else if (event.kind === 'cleaned') {
      setRaw(root.querySelector('[data-import-cleaner]'), event.text || '', 'The cleaner returned no recipe facts.');
      stageStatus('cleaner', 'done');
    } else if (event.kind === 'warning' && event.message && warnings) {
      warnings.appendChild(el('li', null, event.message));
      countNotes();
    } else if (event.kind === 'result') {
      if (status) {
        status.textContent = 'ready';
        status.className = 'codex-status codex-status--connected import-overall-status';
      }
      stageStatus('draft', 'done');
      if (heading) heading.textContent = 'Your recipe is ready';
      if (event.draftUrl && draftLink) draftLink.href = event.draftUrl;
      if (ready) ready.hidden = false;
      if (draftCopy) draftCopy.textContent = 'Ready for your review.';
      openDraft(event.draftUrl);
    } else if (event.kind === 'error') {
      if (status) {
        status.textContent = 'failed';
        status.className = 'codex-status codex-status--missing import-overall-status';
      }
      if (errorBox) {
        errorBox.textContent = event.message || 'The video import failed.';
        errorBox.hidden = false;
      }
      if (heading) heading.textContent = 'Import stopped';
      if (retryBox) retryBox.hidden = false;
      unreadFound();
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
      errorBox.textContent = 'We lost touch with this import, so it may have expired.';
      errorBox.hidden = false;
    }
    if (heading) heading.textContent = 'Import stopped';
    if (retryBox) retryBox.hidden = false;
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
      if (!replayed) { clear(warnings); countNotes(); replayed = true; }
      try { (payload.events || []).forEach(apply); } catch (ignored) { /* keep polling */ }
      since = payload.next || since;
      if (payload.done) { stopped = true; return; }
      window.setTimeout(poll, 900);
    };
    request.send();
  }
  restoreDetails();
  initialiseProgress();
  if (!finished) poll();
}());

/* Step moment stills: one click opens a still in a full-page viewer, with
 * the other stills of the same step one tap or arrow key away. Escape, the
 * close button, or a click outside the image closes it. Without JavaScript
 * the still link opens the image itself. Clicks are delegated, so stills in
 * a chart step swapped in place work too. */
(function () {
  var viewer = null;
  var image = null;
  var previousButton = null;
  var nextButton = null;
  var closeButton = null;
  var links = [];
  var index = 0;
  var opener = null;

  function stillLink(node) {
    while (node && node !== document) {
      if (node.nodeType === 1 && node.hasAttribute('data-still-viewer')) return node;
      node = node.parentNode;
    }
    return null;
  }
  function button(className, label, text, handler) {
    var element = document.createElement('button');
    element.type = 'button';
    element.className = className;
    element.setAttribute('aria-label', label);
    element.textContent = text;
    element.addEventListener('click', function (event) {
      event.stopPropagation();
      handler();
    });
    viewer.appendChild(element);
    return element;
  }
  function build() {
    viewer = document.createElement('div');
    viewer.className = 'still-viewer';
    viewer.hidden = true;
    viewer.setAttribute('role', 'dialog');
    viewer.setAttribute('aria-modal', 'true');
    viewer.setAttribute('aria-label', 'Video frame');
    image = document.createElement('img');
    image.addEventListener('click', function (event) { event.stopPropagation(); });
    viewer.appendChild(image);
    previousButton = button('still-viewer-prev', 'Previous frame', '\u2039', function () { show(index - 1); });
    nextButton = button('still-viewer-next', 'Next frame', '\u203a', function () { show(index + 1); });
    closeButton = button('still-viewer-close', 'Close', '\u00d7', close);
    viewer.addEventListener('click', close);
    document.body.appendChild(viewer);
  }
  function show(next) {
    if (next < 0 || next >= links.length) return;
    index = next;
    var still = links[index].getElementsByTagName('img')[0];
    image.src = links[index].getAttribute('href');
    image.alt = still ? still.alt : '';
    previousButton.hidden = index === 0;
    nextButton.hidden = index === links.length - 1;
  }
  function open(link) {
    if (!viewer) build();
    links = Array.prototype.slice.call(link.parentNode.querySelectorAll('[data-still-viewer]'));
    opener = link;
    show(links.indexOf(link));
    viewer.hidden = false;
    document.body.className += ' still-viewer-open';
    closeButton.focus();
  }
  function close() {
    if (!viewer || viewer.hidden) return;
    viewer.hidden = true;
    image.removeAttribute('src');
    document.body.className = document.body.className.replace(/\s*still-viewer-open/g, '');
    if (opener && document.body.contains(opener)) opener.focus();
    opener = null;
  }

  document.addEventListener('click', function (event) {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    var link = stillLink(event.target);
    if (!link) return;
    event.preventDefault();
    open(link);
  });
  document.addEventListener('keydown', function (event) {
    if (!viewer || viewer.hidden) return;
    var key = event.key || '';
    if (key === 'Escape' || key === 'Esc' || event.keyCode === 27) { event.preventDefault(); close(); }
    else if (key === 'ArrowLeft' || event.keyCode === 37) { event.preventDefault(); show(index - 1); }
    else if (key === 'ArrowRight' || event.keyCode === 39) { event.preventDefault(); show(index + 1); }
  });
}());

/* Library: a ticket's thumbnail pins its full dish photo open; the thumbnail
 * or the open photo closes it again. */
(function () {
  document.addEventListener('click', function (event) {
    var toggle = event.target;
    while (toggle && toggle !== document && !(toggle.hasAttribute && toggle.hasAttribute('data-ticket-photo'))) toggle = toggle.parentNode;
    if (!toggle || toggle === document) return;
    var ticket = toggle;
    while (ticket && !/(^| )ticket( |$)/.test(ticket.className || '')) ticket = ticket.parentNode;
    if (!ticket) return;
    var open = !/(^| )is-photo-open( |$)/.test(ticket.className);
    ticket.className = open ? ticket.className + ' is-photo-open' : ticket.className.replace(/(^| )is-photo-open( |$)/, ' ').replace(/^ +| +$/g, '');
    var button = ticket.querySelector('.ticket-thumb-toggle');
    if (!button) return;
    button.setAttribute('aria-expanded', open ? 'true' : 'false');
    button.setAttribute('aria-label', open ? 'Hide dish photo' : 'Show dish photo');
  });
}());

/* Library: the search runs as you type. A pause of DELAY ms after the last
 * keystroke fetches the library page for the query, and the results replace
 * the heading text and everything below the heading. The form itself stays,
 * so the field keeps its focus and caret. Enter searches at once. A newer
 * search cancels an older one, and the address bar follows the query.
 * Without JavaScript the form submits normally. */
(function () {
  var form = document.querySelector('[data-live-search]');
  var section = document.getElementById('all-recipes');
  var field = form && form.querySelector('input[name="q"]');
  if (!field || !section) return;
  var DELAY = 350;
  var timer = null;
  var request = null;
  var shown = normalize(field.value);

  function normalize(text) {
    return text.replace(/\s+/g, ' ').replace(/^ | $/g, '');
  }
  function urlFor(query) {
    return query ? '/?q=' + encodeURIComponent(query) : '/';
  }
  function render(markup, url) {
    var holder = document.createElement('div');
    holder.innerHTML = markup;
    var fresh = holder.querySelector('#all-recipes');
    var freshHeading = fresh && fresh.querySelector('.search-heading');
    var heading = section.querySelector('.search-heading');
    if (!freshHeading || !heading) { window.location.href = url; return; }
    heading.replaceChild(freshHeading.firstElementChild, heading.firstElementChild);
    while (heading.nextSibling) section.removeChild(heading.nextSibling);
    while (freshHeading.nextSibling) section.appendChild(freshHeading.nextSibling);
    if (window.history && window.history.replaceState) window.history.replaceState({}, '', url);
  }
  function search() {
    if (timer) { window.clearTimeout(timer); timer = null; }
    var query = normalize(field.value);
    if (query === shown) return;
    if (request) request.abort();
    var url = urlFor(query);
    var current = new XMLHttpRequest();
    request = current;
    current.open('GET', url, true);
    current.onreadystatechange = function () {
      if (current.readyState !== 4 || current !== request) return;
      request = null;
      if (current.status >= 200 && current.status < 300) {
        shown = query;
        render(current.responseText, url);
      } else if (current.status) {
        window.location.href = url;
      }
    };
    current.send();
  }

  field.addEventListener('input', function () {
    if (timer) window.clearTimeout(timer);
    timer = window.setTimeout(search, DELAY);
  });
  form.addEventListener('submit', function (event) {
    event.preventDefault();
    search();
  });
}());

/* Library: hovering a ticket on the rail curls up the bottom corner nearest
 * the pointer, as if grabbed. The bend is nothing at the top, then ramps up
 * steeply towards the bottom, so it can't be a CSS transform: each ticket gets
 * an SVG displacement filter (set through --curl) whose strength animates. */
(function () {
  if (!document.querySelector('.ticket-rail') || !window.matchMedia || !window.matchMedia('(hover: hover)').matches) return;
  var NS = 'http://www.w3.org/2000/svg';
  var LIFT = 36;       // displacement scale: the bottom corner rises up to LIFT / 2 px
  var FLAT = 0.4;      // the top 40% of the ticket stays flat
  var DURATION = 260;
  var SPREAD = 0.1;    // the maps reach 10% past each side for paper that bends outwards
  var reduced = window.matchMedia('(prefers-reduced-motion: reduce)');
  var maps = {};
  var count = 0;
  var defs;

  // Two images per side (1 lifts the right corner, -1 the left): a
  // displacement map (red moves pixels sideways, green moves them up) and the
  // faint shading on the curled paper. Both are stretched over the ticket and
  // SPREAD past each side, since curled paper widens beyond its own edges.
  function curlMaps(side) {
    if (maps[side]) return maps[side];
    var w = 160, h = 320;
    var disp = document.createElement('canvas'), shade = document.createElement('canvas');
    disp.width = shade.width = w;
    disp.height = shade.height = h;
    var d = disp.getContext('2d').createImageData(w, h), s = shade.getContext('2d').createImageData(w, h);
    for (var y = 0; y < h; y++) {
      var v = (y + 0.5) / h;
      var ramp = v < FLAT ? 0 : Math.pow((v - FLAT) / (1 - FLAT), 2.6);
      for (var x = 0; x < w; x++) {
        var u = -SPREAD + (x + 0.5) / w * (1 + 2 * SPREAD);
        var across = Math.min(1, Math.max(0, u));
        var lift = ramp * (0.3 + 0.7 * (side > 0 ? across : 1 - across));
        var i = (y * w + x) * 4;
        // Rising paper is foreshortened (read from lower down) and, being
        // nearer the viewer, a little wider (read from nearer the middle).
        d.data[i] = Math.round(255 * (0.5 - (u - 0.5) * 0.4 * lift));
        d.data[i + 1] = Math.round(255 * (0.5 + 0.5 * lift));
        d.data[i + 2] = 128;
        d.data[i + 3] = 255;
        s.data[i] = 17;
        s.data[i + 1] = 26;
        s.data[i + 2] = 34;
        s.data[i + 3] = Math.round(255 * 0.12 * lift);
      }
    }
    disp.getContext('2d').putImageData(d, 0, 0);
    shade.getContext('2d').putImageData(s, 0, 0);
    maps[side] = { disp: disp.toDataURL(), shade: shade.toDataURL() };
    return maps[side];
  }

  function el(name, attrs, parent) {
    var node = document.createElementNS(NS, name);
    for (var key in attrs) node.setAttribute(key, attrs[key]);
    if (parent) parent.appendChild(node);
    return node;
  }

  function curlFor(ticket) {
    if (ticket.curl) return ticket.curl;
    if (!defs) {
      var svg = el('svg', { width: '0', height: '0', 'aria-hidden': 'true', style: 'position:absolute' });
      document.body.appendChild(svg);
      defs = el('defs', {}, svg);
    }
    var id = 'ticket-curl-' + (++count);
    var filter = el('filter', { id: id, filterUnits: 'userSpaceOnUse', primitiveUnits: 'userSpaceOnUse', y: '0', 'color-interpolation-filters': 'sRGB' }, defs);
    var curl = {
      id: id,
      filter: filter,
      dispImage: el('feImage', { y: '0', preserveAspectRatio: 'none', result: 'map' }, filter),
      displace: el('feDisplacementMap', { 'in': 'SourceGraphic', in2: 'map', scale: '0', xChannelSelector: 'R', yChannelSelector: 'G', result: 'bent' }, filter),
      shadeImage: el('feImage', { y: '0', preserveAspectRatio: 'none', result: 'shadeMap' }, filter),
      amount: 0,
      target: 0,
      frame: 0
    };
    var fade = el('feComponentTransfer', { 'in': 'shadeMap', result: 'shade' }, filter);
    curl.shadeAlpha = el('feFuncA', { type: 'linear', slope: '0' }, fade);
    el('feComposite', { 'in': 'shade', in2: 'bent', operator: 'atop' }, filter);
    ticket.curl = curl;
    return curl;
  }

  function setHref(image, href) {
    image.setAttribute('href', href);
    image.setAttributeNS('http://www.w3.org/1999/xlink', 'xlink:href', href);
  }

  function draw(ticket, curl) {
    var eased = curl.amount < 0.5 ? 2 * curl.amount * curl.amount : 1 - Math.pow(-2 * curl.amount + 2, 2) / 2;
    curl.displace.setAttribute('scale', String(LIFT * eased));
    curl.shadeAlpha.setAttribute('slope', String(eased));
    if (curl.amount === 0 && curl.target === 0) ticket.style.removeProperty('--curl');
  }

  function animate(ticket, curl) {
    if (curl.frame) return;
    var last = null;
    function step(now) {
      if (last === null) last = now;
      var delta = reduced.matches ? 1 : (now - last) / DURATION;
      last = now;
      curl.amount = curl.target > curl.amount ? Math.min(curl.target, curl.amount + delta) : Math.max(curl.target, curl.amount - delta);
      draw(ticket, curl);
      curl.frame = curl.amount === curl.target ? 0 : window.requestAnimationFrame(step);
    }
    curl.frame = window.requestAnimationFrame(step);
  }

  // Only tickets hanging from the rail curl; pinned ones show their pin.
  function onRail(ticket) {
    return window.getComputedStyle(ticket, '::before').display === 'none';
  }

  document.addEventListener('mouseenter', function (event) {
    var ticket = event.target;
    if (!ticket.classList || !ticket.classList.contains('ticket') || !onRail(ticket)) return;
    var curl = curlFor(ticket);
    var box = ticket.getBoundingClientRect();
    var width = ticket.offsetWidth, height = ticket.offsetHeight;
    if (curl.amount === 0) {
      var images = curlMaps(event.clientX > box.left + box.width / 2 ? 1 : -1);
      setHref(curl.dispImage, images.disp);
      setHref(curl.shadeImage, images.shade);
    }
    [curl.filter, curl.dispImage, curl.shadeImage].forEach(function (node) {
      node.setAttribute('x', String(-width * SPREAD));
      node.setAttribute('width', String(width * (1 + 2 * SPREAD)));
      node.setAttribute('height', String(height));
    });
    ticket.style.setProperty('--curl', 'url(#' + curl.id + ')');
    curl.target = 1;
    animate(ticket, curl);
  }, true);

  document.addEventListener('mouseleave', function (event) {
    var ticket = event.target;
    if (!ticket.curl) return;
    ticket.curl.target = 0;
    animate(ticket, ticket.curl);
  }, true);
}());

/* Account menu: a click outside the open panel, or Escape, closes it. */
(function () {
  var menu = document.querySelector('[data-account-menu]');
  if (!menu) return;

  function close() {
    menu.removeAttribute('open');
  }

  document.addEventListener('click', function (event) {
    if (menu.hasAttribute('open') && !menu.contains(event.target)) close();
  });
  document.addEventListener('keydown', function (event) {
    if (!menu.hasAttribute('open') || (event.key !== 'Escape' && event.key !== 'Esc')) return;
    close();
    var summary = menu.querySelector('summary');
    if (summary) summary.focus();
  });
}());
