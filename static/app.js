/* ES5-only progressive enhancement: an edit link can open its form without a reload. */
(function () {
  var timerId = null;
  function format(seconds) { var minutes = Math.floor(seconds / 60); var remainder = seconds % 60; return minutes + ':' + (remainder < 10 ? '0' : '') + remainder; }
  function stopTimer() { if (timerId) { window.clearInterval(timerId); timerId = null; } }
  function startTimer() {
    stopTimer();
    var detail = document.querySelector('[data-chart-detail]');
    if (!detail) return;
    var seconds = parseInt(detail.getAttribute('data-timer-seconds'), 10) || 0;
    var output = detail.querySelector('[data-timer-value]');
    if (!seconds || !output) return;
    var deadline = new Date().getTime() + seconds * 1000;
    function tick() { var remaining = Math.max(0, Math.ceil((deadline - new Date().getTime()) / 1000)); output.textContent = format(remaining); if (!remaining) stopTimer(); }
    tick(); timerId = window.setInterval(tick, 250);
  }
  function focusSelected() { var selected = document.querySelector('.chart-cell.is-active'); if (selected && selected.scrollIntoView) selected.scrollIntoView(false); }
  function chartReady() { startTimer(); focusSelected(); }
  function replacePaper(markup, url) {
    var holder = document.createElement('div'); holder.innerHTML = markup;
    var next = holder.querySelector ? holder.querySelector('.paper') : null;
    var paper = document.querySelector('.paper');
    if (!next || !paper) { window.location.href = url; return; }
    stopTimer(); paper.innerHTML = next.innerHTML;
    if (window.history && window.history.replaceState) window.history.replaceState({}, '', url);
    chartReady();
  }
  function navigate(url) {
    var request = new XMLHttpRequest();
    request.open('GET', url, true);
    request.onreadystatechange = function () { if (request.readyState !== 4) return; if (request.status >= 200 && request.status < 300) replacePaper(request.responseText, url); else window.location.href = url; };
    request.send();
  }
  function targetLink(node) { return node.closest ? node.closest('[data-recipe-view], [data-chart-step], [data-chart-start], [data-chart-back], [data-chart-next], [data-chart-exit]') : null; }
  document.addEventListener('click', function (event) {
    var chartLink = targetLink(event.target);
    if (chartLink) { event.preventDefault(); navigate(chartLink.href); return; }
    var link = event.target.closest ? event.target.closest('[data-edit-block]') : null;
    if (!link) return;
    var form = document.getElementById('edit-' + link.getAttribute('data-edit-block'));
    if (!form) return;
    event.preventDefault(); form.hidden = false; form.querySelector('input,textarea').focus();
  });
  document.addEventListener('keydown', function (event) {
    if (event.altKey || event.ctrlKey || event.metaKey || event.target && /input|textarea|select/i.test(event.target.tagName)) return;
    var link = event.keyCode === 37 ? document.querySelector('[data-chart-back]') : event.keyCode === 39 ? document.querySelector('[data-chart-next]') : null;
    if (link) { event.preventDefault(); navigate(link.href); }
  });
  chartReady();
}());

/* Media extraction debugger: polls the run's JSON event feed and appends
 * phase results (description, audio, OCR captures, and the Laguna cleaner
 * output) as they arrive. The page
 * also renders a server-side snapshot; this enhancement only takes over when
 * the snapshot is still empty, so the two never fight over the DOM. */
(function () {
  var root = document.querySelector('[data-debug-root]');
  if (!root || root.getAttribute('data-debug-finished') === '1') return;
  var output = root.querySelector('[data-debug-output]');
  if (!output || output.querySelector('.debug-url-card')) return;
  var eventsUrl = root.getAttribute('data-events-url');
  var framesBase = root.getAttribute('data-frames-base');
  var since = 0;
  var stopped = false;
  var cards = {};

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  function ensureCard(index) {
    var card = cards[index];
    if (card) return card;
    var article = el('section', 'block debug-url-card');
    article.appendChild(el('h2', 'debug-url-title', 'URL #' + (index + 1)));
    var status = el('span', 'codex-status debug-status', 'queued');
    status.setAttribute('data-debug-status', '1');
    article.firstChild.appendChild(status);
    var error = el('p', 'error');
    error.style.display = 'none';
    article.appendChild(error);
    var warnings = el('ul', 'debug-warnings');
    article.appendChild(warnings);
    var phases = {
      description: el('div', 'debug-phase'),
      audio: el('div', 'debug-phase'),
      ocr: el('div', 'debug-phase'),
      cleaner: el('div', 'debug-phase')
    };
    article.appendChild(phases.description);
    article.appendChild(phases.audio);
    article.appendChild(phases.ocr);
    output.appendChild(article);
    card = { article: article, status: status, error: error, warnings: warnings, phases: phases, captureRows: {}, cardsBuilt: false };
    cards[index] = card;
    return card;
  }

  function setDescription(card, data) {
    var box = card.phases.description;
    while (box.firstChild) box.removeChild(box.firstChild);
    box.appendChild(el('h3', null, 'Description captures'));
    if (data.durationSeconds) box.appendChild(el('p', 'muted', 'Duration: ' + data.durationSeconds + 's'));
    if (data.title) box.appendChild(el('p', null, '')).appendChild(el('strong', null, data.title));
    if (data.description) box.appendChild(el('p', 'debug-description', data.description));
    if (!data.title && !data.description) box.appendChild(el('p', 'muted', 'No title or description was captured.'));
  }

  function setAudio(card, data) {
    var box = card.phases.audio;
    while (box.firstChild) box.removeChild(box.firstChild);
    box.appendChild(el('h3', null, 'Audio analysis captures'));
    box.appendChild(el('div', 'debug-mono', data.transcript || '[no transcript]'));
  }

  function setCleaner(card, data) {
    var box = card.phases.cleaner;
    while (box.firstChild) box.removeChild(box.firstChild);
    box.appendChild(el('h3', null, 'Laguna recipe cleaner output'));
    box.appendChild(el('div', 'debug-mono', data.text || '[no cleaned recipe output]'));
  }

  function buildOcrSkeleton(box) {
    while (box.firstChild) box.removeChild(box.firstChild);
    box.appendChild(el('h3', null, 'OCR captures'));
    var table = document.createElement('table');
    table.className = 'debug-captures';
    var head = table.createTHead().insertRow();
    ['Frame', 't', 'Recognised text (kept)', 'Raw engine reading', 'Card'].forEach(function (label) {
      head.appendChild(document.createElement('th')).appendChild(document.createTextNode(label));
    });
    table.appendChild(document.createElement('tbody'));
    box.appendChild(table);
    box.appendChild(el('h3', null, 'On-screen text cards'));
    var list = el('ol', 'debug-final-list');
    box.appendChild(list);
    return { body: table.tBodies[0], list: list };
  }

  function applyCaptureRow(skeleton, index, capture) {
    var row = skeleton.body.insertRow(-1);
    var thumb = row.insertCell(-1);
    thumb.className = 'debug-thumb';
    if (capture.image) {
      var link = document.createElement('a');
      link.href = framesBase + '/' + index + '/' + capture.image;
      link.target = '_blank';
      var img = document.createElement('img');
      img.src = link.href;
      img.alt = 'frame at ' + capture.seconds + 's';
      img.loading = 'lazy';
      link.appendChild(img);
      thumb.appendChild(link);
    } else {
      thumb.appendChild(el('span', 'muted', '\u2014'));
    }
    row.insertCell(-1).className = 'debug-seconds';
    row.cells[row.cells.length - 1].textContent = capture.seconds + 's';
    var clean = row.insertCell(-1);
    clean.className = 'debug-clean';
    if (capture.text !== undefined && capture.text !== null) clean.textContent = capture.text;
    else clean.appendChild(el('span', 'debug-dropped', 'dropped by cleaner'));
    var raw = row.insertCell(-1);
    raw.className = 'debug-mono debug-raw';
    raw.textContent = capture.raw || '';
    var ref = row.insertCell(-1);
    if (capture.card !== undefined && capture.card !== null) {
      ref.appendChild(el('span', 'debug-cardref', '#' + (capture.card + 1)));
    } else {
      ref.appendChild(el('span', 'muted', '\u2014'));
    }
  }

  function setOcr(card, index, data) {
    var skeleton = buildOcrSkeleton(card.phases.ocr);
    (data.captures || []).forEach(function (capture) {
      applyCaptureRow(skeleton, index, capture);
    });
    (data.cards || []).forEach(function (entry) {
      var li = document.createElement('li');
      if (!entry.kept) li.className = 'debug-dropped';
      li.textContent = '[' + entry.seconds + 's] ' + entry.text + (entry.kept ? '' : ' (not sent to the draft)');
      skeleton.list.appendChild(li);
    });
  }

  function apply(event) {
    var kind = event.kind;
    if (kind === 'run-done') { stopped = true; return; }
    if (event.url === undefined || event.url === null) return;
    var index = event.url;
    var card = ensureCard(index);
    if (kind === 'status') {
      card.status.textContent = event.state || 'working';
    } else if (kind === 'description') {
      setDescription(card, event);
    } else if (kind === 'audio') {
      setAudio(card, event);
    } else if (kind === 'cleaned') {
      setCleaner(card, event);
    } else if (kind === 'warning') {
      if (event.message) card.warnings.appendChild(el('li', null, event.message));
    } else if (kind === 'ocr-captures') {
      setOcr(card, index, event);
    } else if (kind === 'result') {
      card.status.textContent = 'done';
      card.status.className = 'codex-status codex-status--connected debug-status';
      if (!card.phases.ocr.hasChildNodes()) {
        card.phases.ocr.appendChild(el('p', 'muted', 'No on-screen text was captured.'));
      }
    } else if (kind === 'error') {
      card.status.textContent = 'failed';
      card.status.className = 'codex-status codex-status--missing debug-status';
      card.error.style.display = '';
      card.error.textContent = event.message || 'Extraction failed.';
    }
  }

  function poll() {
    if (stopped) return;
    var request = new XMLHttpRequest();
    request.open('GET', eventsUrl + '?since=' + since, true);
    request.onreadystatechange = function () {
      if (request.readyState !== 4) return;
      if (request.status >= 200 && request.status < 300) {
        try {
          var payload = JSON.parse(request.responseText);
          (payload.events || []).forEach(apply);
          since = payload.next || since;
          if (payload.done) { stopped = true; return; }
        } catch (error) { /* keep polling */ }
        window.setTimeout(poll, 1500);
      } else {
        window.setTimeout(poll, 4000);
      }
    };
    request.send();
  }

  poll();
}());
