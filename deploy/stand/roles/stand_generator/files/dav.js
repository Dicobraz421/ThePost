// Нагрузка календарей и контактов стенда SMB (APP-D Д.3, Д.4; DEP-21) для k6: календарные операции (CalDAV:
// синхронизация sync-collection, вид недели calendar-query, новое событие), приглашения (событие с участником —
// планирование iTIP на узле) и контакты (CardDAV: синхронизация, новая карточка). Параметры — переменные окружения
// от run-scenario.sh: SMB_HOST, DOMAIN, USERS, USER_PREFIX, ACCOUNTS (login:пароль), DURATION (с), CALENDAR_PER_MIN,
// INVITES_PER_MIN, CONTACTS_PER_MIN. Вход — Basic (пароль приложения). Ответы 5xx — счётчик http_5xx.
import http from 'k6/http';
import encoding from 'k6/encoding';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const E = __ENV;
const BASE = `https://${E.SMB_HOST}`;
const USERS = parseInt(E.USERS, 10);
const DURATION = parseInt(E.DURATION, 10);
const PASS = {};
open(E.ACCOUNTS)
  .split('\n')
  .forEach((line) => {
    const i = line.indexOf(':');
    if (i > 0) PASS[line.slice(0, i)] = line.slice(i + 1);
  });
const http5xx = new Counter('http_5xx');
// Отказ запроса (код не из ожидаемых) по имени; имена — в отчёт criteria.py (dav-summary.json).
const davFail = new Counter('dav_fail');
const NAMES = ['CalDAV sync-collection', 'CalDAV calendar-query', 'CalDAV PUT', 'CalDAV PUT invite',
  'CardDAV sync-collection', 'CardDAV PUT'];
// Подметрики попадают в --summary-export только с порогом: пороги-заглушки, оценка — criteria.py.
const thresholds = { http_5xx: ['count>=0'] };
NAMES.forEach((n) => {
  thresholds[`http_req_duration{name:${n}}`] = ['max>=0'];
  thresholds[`dav_fail{name:${n}}`] = ['count>=0'];
});

function arrival(perMin, exec) {
  return {
    executor: 'constant-arrival-rate',
    rate: Math.max(1, Math.round(parseFloat(perMin) * 60)),
    timeUnit: '1h',
    duration: `${DURATION}s`,
    preAllocatedVUs: 5,
    maxVUs: 100,
    exec,
  };
}

export const options = {
  scenarios: {
    calendar: arrival(E.CALENDAR_PER_MIN, 'calendar'),
    invite: arrival(E.INVITES_PER_MIN, 'invite'),
    contacts: arrival(E.CONTACTS_PER_MIN, 'contacts'),
  },
  summaryTrendStats: ['count', 'avg', 'p(50)', 'p(95)', 'p(99)', 'max'],
  thresholds,
};

function login(n) {
  return `${E.USER_PREFIX}${n}@${E.DOMAIN}`;
}

function randomUser() {
  return 1 + Math.floor(Math.random() * USERS);
}

function collection(kind, n) {
  const name = kind === 'calendars' ? 'calendar' : 'contacts';
  return `${BASE}/dav/${kind}/${encodeURIComponent(login(n)).replace('%40', '@')}/${name}/`;
}

function request(n, method, url, body, headers, name, ok) {
  const h = Object.assign({ Authorization: `Basic ${encoding.b64encode(`${login(n)}:${PASS[login(n)]}`)}` }, headers);
  const r = http.request(method, url, body, { headers: h, tags: { name } });
  if (!check(r, { [`${name} ${ok.join('/')}`]: (x) => ok.includes(x.status) })) davFail.add(1, { name });
  if (r.status >= 500) http5xx.add(1);
  return r;
}

const XML = { 'Content-Type': 'application/xml; charset=utf-8' };
const tokens = {};

// Синхронизация клиента (RFC 6578): с последнего токена учётной записи; первый раз — полная.
function sync(kind, n, name) {
  const key = `${kind}/${n}`;
  const token = tokens[key] ? `<d:sync-token>${tokens[key]}</d:sync-token>` : '<d:sync-token/>';
  const body =
    '<?xml version="1.0" encoding="utf-8"?><d:sync-collection xmlns:d="DAV:">' +
    `${token}<d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>`;
  const r = request(n, 'REPORT', collection(kind, n), body, Object.assign({ Depth: '1' }, XML), name, [207, 403]);
  const m = r.status === 207 && r.body ? r.body.match(/<(?:\w+:)?sync-token>([^<]+)</) : null;
  // 403 valid-sync-token — токен очищен (надгробия старше meta.dav.tombstone_ttl): следующая — полная.
  tokens[key] = m ? m[1] : undefined;
}

function stamp(d) {
  return d.toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '');
}

function uid() {
  return `load-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

function vevent(id, organizer, attendee) {
  const start = new Date(Date.now() + Math.floor(Math.random() * 14 * 24) * 3600 * 1000);
  start.setUTCMinutes(0, 0, 0);
  const end = new Date(start.getTime() + 3600 * 1000);
  const people = attendee
    ? `ORGANIZER:mailto:${organizer}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:${organizer}\r\n` +
      `ATTENDEE;RSVP=TRUE;PARTSTAT=NEEDS-ACTION:mailto:${attendee}\r\n`
    : '';
  return (
    'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//The Post//stand load//RU\r\nBEGIN:VEVENT\r\n' +
    `UID:${id}\r\nDTSTAMP:${stamp(new Date())}\r\nDTSTART:${stamp(start)}\r\nDTEND:${stamp(end)}\r\n` +
    `SUMMARY:Нагрузка стенда\r\n${people}END:VEVENT\r\nEND:VCALENDAR\r\n`
  );
}

function putEvent(n, attendee, name) {
  const id = uid();
  const headers = { 'Content-Type': 'text/calendar; charset=utf-8', 'If-None-Match': '*' };
  const url = `${collection('calendars', n)}${id}.ics`;
  request(n, 'PUT', url, vevent(id, login(n), attendee), headers, name, [201, 204]);
}

// Календарная операция (12 из 15 в сутки, Д.3): синхронизация, вид недели или новое событие.
export function calendar() {
  const n = randomUser();
  const x = Math.random();
  if (x < 0.5) {
    sync('calendars', n, 'CalDAV sync-collection');
  } else if (x < 0.75) {
    const from = new Date();
    const to = new Date(from.getTime() + 7 * 24 * 3600 * 1000);
    const body =
      '<?xml version="1.0" encoding="utf-8"?>' +
      '<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">' +
      '<d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR">' +
      `<c:comp-filter name="VEVENT"><c:time-range start="${stamp(from)}" end="${stamp(to)}"/></c:comp-filter>` +
      '</c:comp-filter></c:filter></c:calendar-query>';
    request(n, 'REPORT', collection('calendars', n), body, Object.assign({ Depth: '1' }, XML),
      'CalDAV calendar-query', [207]);
  } else {
    putEvent(n, null, 'CalDAV PUT');
  }
}

// Приглашение (3 из 15 в сутки, Д.3): событие с участником — другая учётная запись стенда.
export function invite() {
  const n = randomUser();
  let m = randomUser();
  if (USERS > 1) {
    while (m === n) m = randomUser();
  }
  putEvent(n, login(m), 'CalDAV PUT invite');
}

export function contacts() {
  const n = randomUser();
  if (Math.random() < 0.7) {
    sync('addressbooks', n, 'CardDAV sync-collection');
    return;
  }
  const id = uid();
  const body =
    'BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//The Post//stand load//RU\r\n' +
    `UID:${id}\r\nFN:Контакт ${id}\r\nN:${id};Контакт;;;\r\nEMAIL;TYPE=WORK:${id}@external.test\r\nEND:VCARD\r\n`;
  const headers = { 'Content-Type': 'text/vcard; charset=utf-8', 'If-None-Match': '*' };
  request(n, 'PUT', `${collection('addressbooks', n)}${id}.vcf`, body, headers, 'CardDAV PUT', [201, 204]);
}
