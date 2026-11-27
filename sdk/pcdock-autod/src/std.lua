-- Стандартная библиотека сценариев pc-dock: загружается перед каждым сценарием.
-- Здесь только то, что собирается из команд телефона; сами команды (find, tap_on, wait_for, ...) в демоне.

-- Повторить `fn` до `n` раз с паузой `delay` секунд. Успех: первое возвращённое значение, не равное nil/false.
function retry(n, delay, fn)
  local last
  for i = 1, n do
    local ok, res = pcall(fn, i)
    if ok and res then return res end
    last = ok and res or tostring(res)
    if i < n then sleep(delay or 1) end
  end
  return nil, last
end

-- Ссылка (https://...) из текста.
function first_url(text)
  local url = tostring(text or ""):match("https?://[^%s\"<]+")
  if url then url = url:gsub("[%.,%)]+$", "") end
  return url
end

-- Найти первый подходящий из списка целей: возвращает элемент и номер цели.
function find_any(p, targets, timeout)
  local deadline = now() + (timeout or 0)
  repeat
    for i, t in ipairs(targets) do
      local el = p:find_one(t)
      if el then return el, i end
    end
    if timeout and now() < deadline then sleep(0.3) end
  until not timeout or now() >= deadline
  return nil
end

-- Нажать на первый подходящий из списка целей.
function tap_any(p, targets, opts)
  local el, i = find_any(p, targets, opts and opts.timeout or 0)
  if not el then return nil end
  p:tap(el.x, el.y, opts)
  return el, i
end

-- Нажать в поле и набрать текст. По ADB только ASCII: остальное набираем клавишами (латиница).
function type_into(p, target, text, opts)
  local el = p:tap_on(target, opts)
  if not el then return nil end
  sleep(0.2)
  p:type_text(text)
  return el
end

-- Нажатие «как человек»: небольшой разброс точки, силы и длительности.
function human_tap(p, x, y, opts)
  opts = opts or {}
  local j = opts.jitter or 0.01
  local nx = math.min(1, math.max(0, x + (math.random() * 2 - 1) * j))
  local ny = math.min(1, math.max(0, y + (math.random() * 2 - 1) * j))
  p:tap(nx, ny, {
    hold = 0.04 + math.random() * 0.08,
    pressure = 0.5 + math.random() * 0.3,
    size = 0.04 + math.random() * 0.03,
  })
  sleep(0.05 + math.random() * 0.15)
end

-- Свайп «как человек»: разброс концов и времени.
function human_swipe(p, x1, y1, x2, y2, opts)
  opts = opts or {}
  local j = opts.jitter or 0.02
  local function r(v) return math.min(1, math.max(0, v + (math.random() * 2 - 1) * j)) end
  p:swipe(r(x1), r(y1), r(x2), r(y2), {
    duration = (opts.duration or 0.3) * (0.8 + math.random() * 0.5),
    steps = opts.steps or 12,
    pressure = 0.5 + math.random() * 0.3,
  })
  sleep(0.1 + math.random() * 0.2)
end

-- Листать вверх, пока цель не появится (не больше `max` раз).
function scroll_until(p, target, opts)
  opts = opts or {}
  for _ = 1, opts.max or 10 do
    local el = p:find_one(target)
    if el then return el end
    p:swipe(0.5, opts.from or 0.8, 0.5, opts.to or 0.3, {duration = opts.duration or 0.35})
    sleep(opts.pause or 0.6)
  end
  return p:find_one(target)
end

-- Ждать выход в сеть (до `timeout` секунд). Возвращает сведения о сети или nil. Если у прогона задан `check_url`,
-- сеть считается рабочей, когда по нему получен внешний адрес (`public_ip` в ответе).
function wait_online(p, timeout)
  local deadline = now() + (timeout or 30)
  repeat
    local ok, n = pcall(function() return p:network() end)
    if ok and n and n.online then
      if not check_url then return n end
      -- Свой адрес проверки (check_url прогона): сеть есть, когда по нему отвечает внешний адрес.
      local ok2, ip = pcall(function() return p:public_ip() end)
      if ok2 and ip ~= "" then
        n.public_ip = ip
        return n
      end
    end
    sleep(1)
  until now() >= deadline
  return nil
end

-- Перезапуск сети телефона: мобильные данные (по умолчанию) или режим полёта. Возвращает
-- {attempts, old_ip, new_ip, changed}. `opts`: method, attempts, off_time, online_timeout, require_new_ip.
function net_restart(p, opts)
  opts = opts or {}
  local method = opts.method or "mobile_data"
  local before = wait_online(p, 5)
  local old_ip = before and before.local_ip
  local result = {attempts = 0, old_ip = old_ip}
  for i = 1, opts.attempts or 3 do
    result.attempts = i
    if method == "airplane" then p:airplane(true) else p:mobile_data(false) end
    sleep(opts.off_time or 3)
    if method == "airplane" then p:airplane(false) else p:mobile_data(true) end
    local n = wait_online(p, opts.online_timeout or 45)
    if n then
      result.new_ip = n.local_ip
      result.changed = (old_ip ~= nil and n.local_ip ~= old_ip)
      if result.changed or not opts.require_new_ip then return result end
    end
  end
  return nil, result
end
