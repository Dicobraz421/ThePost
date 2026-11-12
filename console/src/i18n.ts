// Русская локаль Refine (SRS 5.2): словарь в сборке, без внешних ресурсов.
import type { I18nProvider } from "@refinedev/core";

const ru: Record<string, string> = {
  "buttons.create": "Создать",
  "buttons.add": "Добавить",
  "buttons.save": "Сохранить",
  "buttons.delete": "Удалить",
  "buttons.edit": "Изменить",
  "buttons.show": "Открыть",
  "buttons.list": "Список",
  "buttons.refresh": "Обновить",
  "buttons.cancel": "Отмена",
  "buttons.confirm": "Вы уверены?",
  "buttons.logout": "Выход",
  "buttons.clear": "Сбросить",
  "buttons.filter": "Фильтр",
  "buttons.notAccessTitle": "Нет прав на это действие",
  "actions.list": "Список",
  "actions.create": "Создание",
  "actions.edit": "Изменение",
  "actions.show": "Просмотр",
  "loading": "Загрузка",
  "notifications.success": "Готово",
  "notifications.error": "Ошибка (код {{statusCode}})",
  "notifications.createSuccess": "Создано: {{resource}}",
  "notifications.createError": "Не удалось создать: {{resource}} (код {{statusCode}})",
  "notifications.deleteSuccess": "Удалено: {{resource}}",
  "notifications.deleteError": "Не удалось удалить: {{resource}} (код {{statusCode}})",
  "notifications.editSuccess": "Сохранено: {{resource}}",
  "notifications.editError": "Не удалось сохранить: {{resource}} (код {{statusCode}})",
  "notifications.undoable": "Отмена возможна ещё {{seconds}} с",
  "pages.error.404": "Страница не найдена",
  "pages.error.backHome": "На главную",
  "pages.error.resource404": "Раздел не найден",
  "pages.error.info": "Возможно, для раздела {{resource}} не создан экран «{{action}}»",
  "warnWhenUnsavedChanges": "Есть несохранённые изменения. Уйти со страницы?",
  "documentTitle.default": "The Post",
  "documentTitle.suffix": " — The Post",
};

export const i18nProvider: I18nProvider = {
  translate: (key: string, options?: unknown, defaultMessage?: string) => {
    const text = ru[key] ?? (typeof options === "string" ? options : defaultMessage) ?? key;
    const vars = options && typeof options === "object" ? (options as Record<string, unknown>) : {};
    return text.replace(/\{\{\s*(\w+)\s*\}\}/g, (_, name: string) => String(vars[name] ?? ""));
  },
  changeLocale: () => undefined,
  getLocale: () => "ru",
};
