// Правила бронирования ресурса (ADM-16, CTR-48): автоматическое принятие, пересечения, длительность и горизонт,
// кому можно бронировать, рабочее время. Поля формы — плоские; в тело запроса и обратно — `toBooking`, `fromBooking`.
import { Checkbox, Form, Input, Radio, Switch } from "antd";

import type { Schemas } from "../api/client";
import { AccountSelect } from "./account-select";
import { GroupSelect } from "./group-select";

export type Booking = Schemas["BookingPolicyBody"];
type Weekday = Schemas["Weekday"];

export type BookingValues = {
  auto_accept: boolean;
  allow_conflicts: boolean;
  max_duration: string;
  horizon: string;
  bookers: "internal" | "only";
  accounts: string[];
  groups: string[];
  hours: boolean;
  timezone: string;
  days: Weekday[];
  from: string;
  to: string;
};

const DAYS: Record<Weekday, string> = {
  monday: "пн",
  tuesday: "вт",
  wednesday: "ср",
  thursday: "чт",
  friday: "пт",
  saturday: "сб",
  sunday: "вс",
};

/** Правила по умолчанию узла (`BookingPolicy::default`). */
export const DEFAULT_BOOKING: Booking = {
  auto_accept: true,
  allow_conflicts: false,
  max_duration: "1d",
  horizon: "180d",
  bookers: { type: "internal" },
  working_hours: null,
};

export function fromBooking(p: Booking = DEFAULT_BOOKING): BookingValues {
  const w = p.working_hours;
  return {
    auto_accept: p.auto_accept,
    allow_conflicts: p.allow_conflicts,
    max_duration: p.max_duration ?? "",
    horizon: p.horizon ?? "",
    bookers: p.bookers.type,
    accounts: p.bookers.type === "only" ? (p.bookers.accounts ?? []) : [],
    groups: p.bookers.type === "only" ? (p.bookers.groups ?? []) : [],
    hours: !!w,
    timezone: w?.timezone ?? "Europe/Moscow",
    days: w?.days ?? ["monday", "tuesday", "wednesday", "thursday", "friday"],
    from: w?.from ?? "09:00",
    to: w?.to ?? "18:00",
  };
}

export function toBooking(v: BookingValues): Booking {
  return {
    auto_accept: v.auto_accept,
    allow_conflicts: v.allow_conflicts,
    max_duration: v.max_duration.trim() || null,
    horizon: v.horizon.trim() || null,
    bookers: v.bookers === "only" ? { type: "only", accounts: v.accounts ?? [], groups: v.groups ?? [] } : { type: "internal" },
    working_hours: v.hours ? { timezone: v.timezone.trim(), days: v.days, from: v.from.trim(), to: v.to.trim() } : null,
  };
}

const HHMM = { pattern: /^\d{2}:\d{2}$/, message: "ЧЧ:ММ" };
const DURATION = { pattern: /^\s*(\d+[dhms])*\s*$/, message: "Например 30m, 4h, 1h30m, 180d; пусто — без ограничения" };

/** Поля правил бронирования внутри `Form<BookingValues>` (или формы с такими полями). */
export function BookingPolicyFields() {
  const form = Form.useFormInstance<BookingValues>();
  const only = Form.useWatch("bookers", form) === "only";
  const hours = Form.useWatch("hours", form);
  return (
    <>
      <Form.Item
        name="auto_accept"
        label="Принимать приглашения автоматически"
        valuePropName="checked"
        extra="Выключено — подходящее приглашение остаётся предварительным до решения представителя ресурса."
      >
        <Switch />
      </Form.Item>
      <Form.Item name="allow_conflicts" label="Разрешить пересечения" valuePropName="checked">
        <Switch />
      </Form.Item>
      <Form.Item name="max_duration" label="Наибольшая длительность встречи" rules={[DURATION]} extra="Пусто — без ограничения.">
        <Input style={{ maxWidth: 200 }} placeholder="4h" />
      </Form.Item>
      <Form.Item name="horizon" label="Бронировать вперёд не более" rules={[DURATION]} extra="Пусто — без ограничения.">
        <Input style={{ maxWidth: 200 }} placeholder="180d" />
      </Form.Item>
      <Form.Item name="bookers" label="Кто может бронировать">
        <Radio.Group
          options={[
            { value: "internal", label: "любая учётная запись организации" },
            { value: "only", label: "только перечисленные" },
          ]}
        />
      </Form.Item>
      {only && (
        <>
          <Form.Item name="accounts" label="Учётные записи">
            <AccountSelect />
          </Form.Item>
          <Form.Item name="groups" label="Группы">
            <GroupSelect />
          </Form.Item>
        </>
      )}
      <Form.Item name="hours" label="Только в рабочее время" valuePropName="checked" extra="Выключено — круглосуточно.">
        <Switch />
      </Form.Item>
      {hours && (
        <>
          <Form.Item name="timezone" label="Часовой пояс" rules={[{ required: true, whitespace: true, message: "Укажите часовой пояс IANA" }]}>
            <Input style={{ maxWidth: 260 }} placeholder="Europe/Moscow" />
          </Form.Item>
          <Form.Item name="days" label="Рабочие дни" rules={[{ required: true, message: "Выберите дни" }]}>
            <Checkbox.Group options={Object.entries(DAYS).map(([value, label]) => ({ value, label }))} />
          </Form.Item>
          <Form.Item name="from" label="Начало дня" rules={[{ required: true, message: "ЧЧ:ММ" }, HHMM]}>
            <Input style={{ maxWidth: 120 }} placeholder="09:00" />
          </Form.Item>
          <Form.Item name="to" label="Конец дня" rules={[{ required: true, message: "ЧЧ:ММ" }, HHMM]}>
            <Input style={{ maxWidth: 120 }} placeholder="18:00" />
          </Form.Item>
        </>
      )}
    </>
  );
}

/** Правила одной строкой для списка ресурсов. */
export function bookingSummary(p?: Booking | null) {
  if (!p) return "не заданы";
  const parts = [p.auto_accept ? "автопринятие" : "по решению представителя"];
  if (p.allow_conflicts) parts.push("пересечения разрешены");
  if (p.max_duration) parts.push(`до ${p.max_duration}`);
  if (p.bookers.type === "only") parts.push("ограниченный круг");
  if (p.working_hours) parts.push(`${p.working_hours.from}–${p.working_hours.to}`);
  return parts.join(", ");
}
