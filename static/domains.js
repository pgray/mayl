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
      r.className = 'ok';
      r.textContent = 'Token: ' + d.token;
      i.value = '';
      location.reload();
    } else {
      r.className = 'err';
      r.textContent = d.error;
    }
  } catch (ex) {
    r.className = 'err';
    r.textContent = ex.message;
  }
}
