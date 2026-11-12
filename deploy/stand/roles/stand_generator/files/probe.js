// Зонды JMAP стенда SMB для k6 (APP-D Д.7, DEP-13). Каждый замер — строка JSON в журнал консоли
// (--log-format raw --console-output <файл>): {"t": unix, "probe": имя, "ms": задержка, "ok": bool}.
//   MODE=run  — jmap_query и jmap_get (Email/query + Email/get случайного ящика нагрузки), fts (Email/query по
//               слову в ящике BIG), signal_push (Email/set ключевого слова → StateChange push на WebSocket, probe3);
//   MODE=idle — IDLE_N простаивающих соединений push (RFC 8887) двумя половинами до IDLE_UNTIL (unix, с; память
//               на соединение — прирост RSS от первой половины ко второй).
// Окружение от run-scenario.sh: SMB_HOST, DOMAIN, USERS, USER_PREFIX, ACCOUNTS, WORDS_FILE, BIG, INTERVAL, DURATION.
import http from 'k6/http';
import ws from 'k6/ws';
import encoding from 'k6/encoding';
import { sleep } from 'k6';

const E = __ENV;
const BASE = `https://${E.SMB_HOST}`;
const USERS = parseInt(E.USERS, 10);
const INTERVAL = parseFloat(E.INTERVAL || '1');
const DURATION = parseInt(E.DURATION || '60', 10);
const WORDS = open(E.WORDS_FILE).split('\n').filter((w) => w.length > 0);
const PASS = {};
open(E.ACCOUNTS)
  .split('\n')
  .forEach((line) => {
    const i = line.indexOf(':');
    if (i > 0) PASS[line.slice(0, i)] = line.slice(i + 1);
  });
const USING = ['urn:ietf:params:jmap:core', 'urn:ietf:params:jmap:mail'];

function login(name) {
  return name.includes('@') ? name : `${name}@${E.DOMAIN}`;
}

function auth(name) {
  const l = login(name);
  return { Authorization: `Basic ${encoding.b64encode(`${l}:${PASS[l]}`)}` };
}

function log(probe, ms, ok, extra) {
  console.log(JSON.stringify(Object.assign({ t: Date.now() / 1000, probe, ms, ok }, extra || {})));
}

const accounts = {};

function account(name) {
  if (!(name in accounts)) {
    const r = http.get(`${BASE}/jmap/session`, { headers: auth(name), tags: { name: 'session' } });
    if (r.status !== 200) return null;
    accounts[name] = r.json('primaryAccounts')['urn:ietf:params:jmap:mail'];
  }
  return accounts[name];
}

function api(name, calls, tag) {
  const acc = account(name);
  if (acc === null) return null;
  const body = { using: USING, methodCalls: calls.map((c, i) => [c[0], Object.assign({ accountId: acc }, c[1]), `${i}`]) };
  const headers = Object.assign({ 'Content-Type': 'application/json' }, auth(name));
  return http.post(`${BASE}/jmap/api`, JSON.stringify(body), { headers, tags: { name: tag } });
}

const every = { executor: 'constant-arrival-rate', rate: 1, preAllocatedVUs: 2, maxVUs: 20 };
const RUN = {
  read: Object.assign({ exec: 'read', timeUnit: `${Math.round(INTERVAL * 1000)}ms`, duration: `${DURATION}s` }, every),
  fts: Object.assign({ exec: 'fts', timeUnit: `${Math.round(INTERVAL * 1000)}ms`, duration: `${DURATION}s` }, every),
  push: { executor: 'per-vu-iterations', vus: 1, iterations: 1, maxDuration: `${DURATION + 30}s`, exec: 'push' },
};
// Две половины по IDLE_N / 2: вторая — через IDLE_HOLD / 2 с после первой; все закрываются в IDLE_UNTIL.
const IDLE_HALF = Math.max(1, Math.floor(parseInt(E.IDLE_N || '2', 10) / 2));
const IDLE_HOLD = parseInt(E.IDLE_HOLD || '60', 10);
const IDLE = {
  idle_a: { executor: 'per-vu-iterations', vus: IDLE_HALF, iterations: 1, maxDuration: `${IDLE_HOLD + 120}s`, exec: 'idle', env: { HALF: 'a' } },
  idle_b: {
    executor: 'per-vu-iterations',
    vus: IDLE_HALF,
    iterations: 1,
    startTime: `${10 + Math.floor(IDLE_HOLD / 2)}s`,
    maxDuration: `${IDLE_HOLD + 120}s`,
    exec: 'idle',
    env: { HALF: 'b' },
  },
};

export const options = { scenarios: E.MODE === 'idle' ? IDLE : RUN, summaryTrendStats: ['p(95)', 'p(99)', 'max'] };

export function read() {
  const name = `${E.USER_PREFIX}${1 + Math.floor(Math.random() * USERS)}`;
  const q = api(name, [['Email/query', { sort: [{ property: 'receivedAt', isAscending: false }], limit: 20 }]], 'Email/query');
  if (q === null) return log('jmap_query', null, false, { err: 'session' });
  const qok = q.status === 200;
  log('jmap_query', q.timings.duration, qok);
  const ids = qok ? q.json('methodResponses.0.1.ids') || [] : [];
  if (ids.length === 0) return;
  const g = api(name, [['Email/get', { ids, properties: ['subject', 'from', 'receivedAt', 'preview', 'keywords'] }]], 'Email/get');
  log('jmap_get', g.timings.duration, g.status === 200);
}

export function fts() {
  const w = WORDS[Math.floor(Math.random() * WORDS.length)];
  const r = api(E.BIG, [['Email/query', { filter: { text: w }, limit: 50 }]], 'Email/query text');
  if (r === null) return log('fts', null, false, { err: 'session' });
  log('fts', r.timings.duration, r.status === 200);
}

// Push probe3: раз в INTERVAL — смена $flagged первого письма; задержка — до первого StateChange после смены.
export function push() {
  const q = api('probe3', [['Email/query', { limit: 1 }]], 'Email/query');
  const ids = q && q.status === 200 ? q.json('methodResponses.0.1.ids') || [] : [];
  if (ids.length === 0) return log('signal_push', null, false, { err: 'нет письма probe3' });
  const id = ids[0];
  const end = Date.now() + DURATION * 1000;
  let pending = 0;
  let on = false;
  const params = { headers: Object.assign({ 'Sec-WebSocket-Protocol': 'jmap' }, auth('probe3')) };
  const res = ws.connect(`wss://${E.SMB_HOST}/jmap/ws`, params, (socket) => {
    socket.on('open', () => {
      socket.send(JSON.stringify({ '@type': 'WebSocketPushEnable', dataTypes: null }));
      socket.setInterval(() => {
        if (Date.now() > end) return socket.close();
        if (pending > 0) log('signal_push', null, false, { err: 'нет StateChange' });
        on = !on;
        pending = Date.now();
        api('probe3', [['Email/set', { update: { [id]: { 'keywords/$flagged': on ? true : null } } }]], 'Email/set keywords');
      }, INTERVAL * 1000);
    });
    socket.on('message', (m) => {
      if (pending > 0 && m.includes('StateChange')) {
        log('signal_push', Date.now() - pending, true);
        pending = 0;
      }
    });
    socket.on('error', (e) => log('signal_push', null, false, { err: `${e.error()}` }));
  });
  if (!res || res.status !== 101) log('signal_push', null, false, { err: `ws ${res ? res.status : 'нет ответа'}` });
}

export function idle() {
  const name = `${E.USER_PREFIX}${1 + ((__VU - 1) % USERS)}`;
  // Половина соединений открывается за ~10 с; окна замера — после последнего push_open каждой половины.
  sleep(Math.random() * 10);
  const params = { headers: Object.assign({ 'Sec-WebSocket-Protocol': 'jmap' }, auth(name)) };
  const until = parseInt(E.IDLE_UNTIL, 10) * 1000;
  const res = ws.connect(`wss://${E.SMB_HOST}/jmap/ws`, params, (socket) => {
    socket.on('open', () => {
      socket.send(JSON.stringify({ '@type': 'WebSocketPushEnable', dataTypes: null }));
      log('push_open', null, true, { half: __ENV.HALF });
      socket.setTimeout(() => socket.close(), Math.max(1000, until - Date.now()));
    });
  });
  if (!res || res.status !== 101) log('push_open', null, false, { err: `ws ${res ? res.status : 'нет ответа'}` });
}
