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
