// Экран, недоступный роли (UI-14): прямой адрес раздела или действия, на которое узел ответит отказом.
import { Button, Result } from "antd";
import { useNavigate } from "react-router";

export function Forbidden() {
  const navigate = useNavigate();
  return (
    <Result
      status="403"
      title="Нет доступа"
      subTitle="Этот экран недоступен вашей роли администратора."
      extra={
        <Button type="primary" onClick={() => void navigate("/")}>
          На начальный экран
        </Button>
      }
    />
  );
}
