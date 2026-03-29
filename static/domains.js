async function addDomain(e) {
  e.preventDefault();
  const r = document.getElementById('domain-result');
  const i = document.getElementById('domain-input');
  r.className = '';
  r.textContent = '';
  try {
    const res = await fetch('/domains', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ domain: i.value }),
    });
    const d = await res.json();
    if (res.ok) {
      i.value = '';
      showTokenModal(d.domain, d.token);
    } else {
      r.className = 'err';
      r.textContent = d.error;
    }
  } catch (ex) {
    r.className = 'err';
    r.textContent = ex.message;
  }
}

function showTokenModal(domain, token) {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML =
    '<div class="modal">' +
      '<h3>' + domain + '</h3>' +
      '<p class="modal-token">' + token + '</p>' +
      '<p class="modal-hint">copy this token — it won\'t be shown again</p>' +
      '<button onclick="this.closest(\'.modal-overlay\').remove();location.reload()">dismiss</button>' +
    '</div>';
  document.body.appendChild(overlay);
  // select the token text for easy copying
  const range = document.createRange();
  range.selectNodeContents(overlay.querySelector('.modal-token'));
  window.getSelection().removeAllRanges();
  window.getSelection().addRange(range);
}
