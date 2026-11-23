// The chat page. The extension owns the conversation: it sends the whole
// transcript when the page loads (`reset`, then `items`), and each item as it
// changes (`item`); the page draws them, and sends what the user does.
// Markdown arrives as HTML the extension made safe; everything else is set
// as text.
'use strict'

const $ = id => document.getElementById(id)
const transcript = $('transcript')
const input = $('input')
const sendButton = $('send')
const popup = $('popup')
const contextChip = $('context')

/** The last status: busy, models, mode, commands, problem, context. */
let status = { busy: false, models: [], commands: [], context: {} }
/** Whether a `reset` came: messages queued for an earlier page come first. */
let loaded = false
/** Items' elements, by id. */
const elements = new Map()
/** The folder's files, once asked for, for @-mentions. */
let files = null
/** Whether the editor's context goes with the next message. */
let contextEnabled = null
/** What the message box completes: `{ kind, start, items, index }`. */
let completion = null

const send = message => flareite.postMessage(message)

// --- Building elements ---

function el(tag, className, text) {
  const element = document.createElement(tag)
  if (className) element.className = className
  if (text !== undefined && text !== null) element.textContent = text
  return element
}

function button(label, onClick, primary) {
  const element = el('button', 'flareite-button' + (primary ? ' flareite-primary' : ''), label)
  element.type = 'button'
  element.addEventListener('click', onClick)
  return element
}

function nearBottom() {
  return transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 80
}

function scrollToEnd() {
  transcript.scrollTop = transcript.scrollHeight
}

// --- Items ---

const renderers = {
  user(item) {
    const node = el('div', 'item user')
    if (item.context) node.append(el('div', 'user__context', '⧉ ' + item.context))
    node.append(el('div', 'user__text', item.text))
    return node
  },

  text(item) {
    const node = el('div', 'item markdown' + (item.streaming ? ' streaming' : ''))
    node.innerHTML = item.html
    markPaths(node)
    return node
  },

  thinking(item) {
    const node = el('details', 'item thinking')
    node.append(el('summary', '', '✻ Thinking'), el('pre', 'detail', item.text))
    return node
  },

  tool(item) {
    const node = el('details', 'item tool')
    const summary = el('summary')
    summary.append(el('span', 'dot dot--' + item.status), el('span', 'tool__title', item.title))
    if (item.status === 'stopped') summary.append(el('span', 'muted', '(stopped)'))
    node.append(summary)
    if (item.detail) node.append(el('pre', 'detail', item.detail))
    if (item.output) node.append(el('pre', 'output', item.output))
    if (!item.detail && !item.output)
      node.append(el('div', 'muted', item.status === 'running' ? 'Running…' : 'No output.'))
    return node
  },

  permission(item) {
    const node = el('div', 'item card' + (item.state === 'pending' ? '' : ' card--decided'))
    const head = el('div', 'card__head')
    head.append(el('span', '', item.title))
    if (item.state !== 'pending') {
      head.append(
        el(
          'span',
          'card__state',
          { allowed: '✓ Allowed', denied: '✕ Denied', cancelled: 'Cancelled' }[item.state] ||
            item.state,
        ),
      )
    }
    node.append(head)
    const body = el('div', 'card__body')
    if (item.reason) body.append(el('div', 'muted', item.reason))
    if (item.description) body.append(el('div', 'muted', item.description))
    if (item.command) body.append(el('pre', 'detail', item.command))
    if (item.input) body.append(el('pre', 'detail', item.input))
    if (item.diff) body.append(diffView(item.diff, item.state === 'pending'))
    if (item.plan) {
      const plan = el('div', 'markdown')
      plan.innerHTML = item.plan
      markPaths(plan)
      body.append(plan)
    }
    let questions = null
    if (item.questions) {
      if (item.state === 'pending') questions = questionsPager(item.questions, body)
      else body.append(answeredView(item))
    }
    node.append(body)
    if (item.state === 'pending') node.append(permissionButtons(item, questions))
    return node
  },

  note(item) {
    const node = el('div', 'item note note--' + (item.level || 'info'))
    node.append(el(item.preformatted ? 'pre' : 'p', '', item.text))
    if (item.resetsAt) {
      const at = new Date(item.resetsAt * 1000)
      node.append(el('p', 'muted', 'It resets at ' + at.toLocaleString() + '.'))
    }
    if (item.actions && item.actions.length) {
      const row = el('div', 'card__buttons')
      for (const action of item.actions) {
        row.append(button(action.label, () => send({ type: 'action', name: action.action })))
      }
      node.append(row)
    }
    return node
  },

  result(item) {
    return el('div', 'item result', '✓ ' + item.text)
  },
}

function permissionButtons(item, questions) {
  const row = el('div', 'card__buttons')
  const feedback = el('input', 'flareite-input feedback')
  feedback.placeholder = 'Tell Claude what to do instead (optional)'
  const decide = (decision, extra) => send({ type: 'permission', id: item.id, decision, ...extra })
  const deny = () => decide('deny', { message: feedback.value })
  if (item.tool === 'AskUserQuestion' && questions) {
    const answer = button(
      'Answer',
      () => {
        const answers = questions.answers()
        if (answers) decide('allow', { answers })
      },
      true,
    )
    const sync = () => {
      answer.disabled = !questions.answers()
      answer.title = answer.disabled ? 'Answer every question first' : ''
    }
    questions.onChange(sync, () => {
      if (!answer.disabled) answer.click()
    })
    sync()
    row.append(answer, button('Skip', deny))
  } else if (item.tool === 'ExitPlanMode') {
    row.append(
      button('Approve', () => decide('allow'), true),
      button('Approve, Edit Automatically', () => {
        decide('allow')
        send({ type: 'mode', value: 'acceptEdits' })
      }),
      button('Keep Planning', deny),
      feedback,
    )
  } else {
    row.append(button(item.diff ? 'Accept' : 'Allow', () => decide('allow'), true))
    if (item.allowSession || item.diff) {
      row.append(
        button(item.diff ? 'Accept All Edits' : 'Allow for This Chat', () =>
          decide('allowSession'),
        ),
      )
    }
    row.append(button(item.diff ? 'Reject' : 'Deny', deny), feedback)
  }
  feedback.addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      event.preventDefault()
      deny()
    }
  })
  return row
}

function diffView(diff, pending) {
  const node = el('div', 'diff')
  const head = el('div', 'diff__head')
  const path = el('span', 'diff__path', diff.path)
  head.append(
    path,
    el('span', 'added', '+' + diff.added),
    el('span', 'removed', '−' + diff.removed),
  )
  if (diff.created) head.append(el('span', 'muted', 'new file'))
  head.append(el('span', 'spacer'))
  if (pending && diff.edit)
    head.append(button('Open Diff', () => send({ type: 'openDiff', edit: diff.edit })))
  node.append(head)
  const body = el('div', 'diff__body')
  const table = el('table')
  diff.hunks.forEach((hunk, index) => {
    if (index > 0) {
      const gap = el('tr', 'gap')
      const cell = el('td', '', '⋯')
      cell.colSpan = 4
      gap.append(cell)
      table.append(gap)
    }
    for (const [kind, oldLine, newLine, text] of hunk.lines) {
      const row = el('tr', kind === '+' ? 'add' : kind === '-' ? 'del' : 'ctx')
      row.append(
        el('td', 'number', oldLine ?? ''),
        el('td', 'number', newLine ?? ''),
        el('td', 'sign', kind === ' ' ? '' : kind),
        el('td', 'code', text),
      )
      table.append(row)
    }
  })
  body.append(table)
  node.append(body)
  if (diff.approximate) {
    node.append(
      el(
        'div',
        'diff__note',
        "Only the replaced text is shown: the file doesn't contain it as it is.",
      ),
    )
  }
  if (diff.truncated) node.append(el('div', 'diff__note', 'The rest is in Open Diff.'))
  return node
}

/**
 * Draws questions to answer, one page each. "Other" is a choice of its own:
 * its text is sent only while it's chosen. Returns `answers()` (null while a
 * question has none) and `onChange(changed, submit)`.
 */
function questionsPager(questions, container) {
  const node = el('div', 'questions')
  const tabs = el('div', 'questions__tabs')
  tabs.setAttribute('role', 'tablist')
  const pages = []
  let current = 0
  let changed = () => {}
  let submit = () => {}

  const answerOf = page => {
    const picked = page.inputs.filter(input => input.checked).map(input => input.value)
    if (page.other.checked && page.otherText.value.trim()) picked.push(page.otherText.value.trim())
    return picked.length ? picked.join(', ') : null
  }
  const nav = el('div', 'questions__nav')
  const back = button('Back', () => show(current - 1))
  const next = button('Next', () => show(current + 1))
  const count = el('span', 'muted', '')
  nav.append(back, next, count)

  const show = index => {
    current = Math.max(0, Math.min(pages.length - 1, index))
    pages.forEach((page, at) => {
      page.block.hidden = at !== current
      page.tab.setAttribute('aria-selected', String(at === current))
      page.tab.tabIndex = at === current ? 0 : -1
    })
    back.disabled = current === 0
    next.disabled = current === pages.length - 1
    count.textContent = `${current + 1} of ${pages.length}`
  }
  const update = () => {
    for (const page of pages) {
      page.tab.classList.toggle('questions__tab--answered', Boolean(answerOf(page)))
    }
    changed()
  }
  /** After a choice: the next unanswered question, if any. */
  const advance = () => {
    const after = pages.findIndex((page, at) => at > current && !answerOf(page))
    const target = after >= 0 ? after : pages.findIndex(page => !answerOf(page))
    if (target >= 0 && target !== current) show(target)
  }

  questions.forEach((question, index) => {
    const block = el('div', 'question')
    block.setAttribute('role', 'tabpanel')
    if (question.header) block.append(el('div', 'question__header', question.header))
    block.append(el('div', 'question__text', question.question))
    const name = 'q' + index + '-' + Math.random().toString(36).slice(2)
    const type = question.multiSelect ? 'checkbox' : 'radio'
    const inputs = []
    for (const option of question.options || []) {
      const label = el('label', 'option')
      const choice = el('input')
      choice.type = type
      choice.name = name
      choice.value = option.label
      const text = el('span', '', option.label)
      if (option.description) text.append(el('small', '', option.description))
      label.append(choice, text)
      block.append(label)
      inputs.push(choice)
      choice.addEventListener('change', () => {
        update()
        // One choice answers a single-choice question: on to the next.
        if (!question.multiSelect && choice.checked) setTimeout(advance, 150)
      })
    }
    const otherRow = el('label', 'option option--other')
    const other = el('input')
    other.type = type
    other.name = name
    other.value = ''
    other.setAttribute('aria-label', 'Other')
    const otherText = el('input', 'flareite-input other')
    otherText.placeholder = 'Other…'
    otherRow.append(other, otherText)
    block.append(otherRow)
    other.addEventListener('change', () => {
      if (other.checked) otherText.focus()
      update()
    })
    // Typing chooses Other.
    otherText.addEventListener('input', () => {
      if (otherText.value.trim() && !other.checked) other.checked = true
      update()
    })
    otherText.addEventListener('keydown', event => {
      if (event.key !== 'Enter') return
      event.preventDefault()
      if (pages.every(answerOf)) submit()
      else advance()
    })

    const tab = el('button', 'questions__tab', question.header || 'Question ' + (index + 1))
    tab.type = 'button'
    tab.setAttribute('role', 'tab')
    tab.title = question.question
    tab.addEventListener('click', () => show(index))
    tabs.append(tab)
    pages.push({ block, inputs, other, otherText, tab, question: question.question })
  })

  tabs.addEventListener('keydown', event => {
    if (event.key !== 'ArrowRight' && event.key !== 'ArrowLeft') return
    event.preventDefault()
    show(current + (event.key === 'ArrowRight' ? 1 : -1))
    pages[current].tab.focus()
  })

  if (pages.length > 1) node.append(tabs)
  for (const page of pages) node.append(page.block)
  if (pages.length > 1) node.append(nav)
  container.append(node)
  show(0)
  update()

  return {
    answers() {
      const answers = {}
      for (const page of pages) {
        const answer = answerOf(page)
        if (!answer) return null
        answers[page.question] = answer
      }
      return answers
    },
    onChange(onChanged, onSubmit) {
      changed = onChanged
      submit = onSubmit
    },
  }
}

/** A decided question card: each question with the answer given. */
function answeredView(item) {
  const node = el('div', 'questions questions--answered')
  const answers = item.answers || {}
  for (const question of item.questions) {
    const block = el('div', 'question')
    if (question.header) block.append(el('div', 'question__header', question.header))
    block.append(el('div', 'question__text', question.question))
    const answer = answers[question.question]
    if (answer) block.append(el('div', 'answer', answer))
    else if (item.state !== 'allowed') {
      block.append(el('div', 'answer answer--none', 'Not answered'))
    }
    node.append(block)
  }
  return node
}

/** Inline code naming a file (`src/main.rs:12`) opens it when clicked. */
function markPaths(node) {
  for (const code of node.querySelectorAll(':not(pre) > code')) {
    if (
      /^[\w.@~-]*[\w-][\w.\/\\-]*\.[A-Za-z0-9]{1,8}(:\d+){0,2}$/.test(code.textContent) &&
      !/^\d/.test(code.textContent)
    ) {
      code.classList.add('path')
      code.title = 'Open'
    }
  }
}

/** A path with `:line` or `#L12` after it, split. */
function parseFileReference(text) {
  let match = /^(.*?)#L(\d+)(?:-\d+)?$/.exec(text)
  if (match) return { path: match[1], line: Number(match[2]) }
  match = /^(.*?):(\d+)(?::\d+)?$/.exec(text)
  if (match && !/^[A-Za-z]$/.test(match[1])) return { path: match[1], line: Number(match[2]) }
  return { path: text, line: null }
}

function put(item) {
  const render = renderers[item.kind]
  if (!render) return
  // Keep a question's choices, and a feedback being typed, while unchanged.
  const existing = elements.get(item.id)
  if (existing && existing.dataset.state === 'pending' && item.state === 'pending') return
  const stick = nearBottom()
  const node = render(item)
  node.dataset.id = item.id
  if (item.state) node.dataset.state = item.state
  if (existing) {
    // Opened tool rows stay open.
    if (existing.open) node.open = true
    existing.replaceWith(node)
  } else {
    transcript.append(node)
  }
  elements.set(item.id, node)
  $('empty').hidden = true
  if (stick) scrollToEnd()
}

function reset() {
  for (const node of elements.values()) node.remove()
  elements.clear()
  $('empty').hidden = false
}

// --- Status ---

function setStatus(next) {
  status = next
  $('title').textContent = next.title || 'Claude Code'
  $('account').textContent = next.account ? '· ' + next.account : ''
  const problem = $('problem')
  problem.replaceChildren()
  problem.hidden = !next.problem
  if (next.problem) {
    problem.append(el('p', '', next.problem.text))
    for (const action of next.problem.actions) {
      problem.append(
        button(action.label, () => send({ type: 'action', name: action.name || action.action })),
      )
    }
  }
  const model = $('model')
  model.replaceChildren()
  const models = next.models.length ? next.models : [{ value: 'default', label: 'Default model' }]
  for (const choice of models) {
    const option = el('option', '', choice.label)
    option.value = choice.value
    option.title = choice.description || ''
    model.append(option)
  }
  if (!models.some(choice => choice.value === next.model)) {
    const option = el('option', '', next.model)
    option.value = next.model
    model.append(option)
  }
  model.value = next.model
  setEffort(next, models)
  setModes(next, models)
  $('busy').hidden = !next.busy
  setContext(next.context)
  updateSend()
}

/** The permission modes: value, label, and what it does. */
const MODES = [
  ['default', 'Ask before edits', 'Claude asks before it changes a file or runs a command'],
  ['acceptEdits', 'Edit automatically', 'Claude changes files without asking; commands still ask'],
  [
    'plan',
    'Plan mode',
    'Claude plans without changing anything, then asks you to approve the plan',
  ],
  ['auto', 'Auto mode', 'Claude Code decides which actions are safe to take without asking'],
  [
    'bypassPermissions',
    'Bypass permissions',
    'Claude runs commands and changes files without asking (asks you to allow it the first time)',
  ],
]

/** The mode menu; Auto mode only for models that have it. */
function setModes(next, models) {
  const mode = $('mode')
  const current = models.find(choice => choice.value === next.model) || models[0]
  const auto = Boolean(current && current.auto)
  mode.replaceChildren()
  for (const [value, label, title] of MODES) {
    if (value === 'auto' && !auto && next.mode !== 'auto') continue
    const option = el('option', '', label)
    option.value = value
    option.title = title
    mode.append(option)
  }
  if (!MODES.some(([value]) => value === next.mode)) {
    const option = el('option', '', next.mode)
    option.value = next.mode
    mode.append(option)
  }
  mode.value = next.mode
  const chosen = MODES.find(([value]) => value === next.mode)
  mode.title = chosen ? chosen[2] : 'How Claude asks before acting'
}

const EFFORT_NAMES = {
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  xhigh: 'Extra high',
  max: 'Max',
}

function effortName(level) {
  return EFFORT_NAMES[level] || level
}

/** The effort menu: the levels the chosen model takes, hidden if it has none. */
function setEffort(next, models) {
  const effort = $('effort')
  const current = models.find(choice => choice.value === next.model) || models[0]
  const levels = [...((current && current.efforts) || [])]
  if (next.effort && !levels.includes(next.effort)) levels.push(next.effort)
  effort.hidden = levels.length === 0
  effort.replaceChildren()
  const fallback = next.appliedEffort && !next.effort ? ` (${effortName(next.appliedEffort)})` : ''
  const automatic = el('option', '', 'Default effort' + fallback)
  automatic.value = ''
  automatic.title = "The model's level, or the one in your Claude Code settings"
  effort.append(automatic)
  for (const level of levels) {
    const option = el('option', '', effortName(level) + ' effort')
    option.value = level
    effort.append(option)
  }
  effort.value = next.effort || ''
}

function setContext(context) {
  status.context = context || {}
  if (contextEnabled === null && context) contextEnabled = context.enabled !== false
  const label = status.context.label
  contextChip.hidden = !label
  contextChip.textContent = '⧉ ' + (label || '')
  contextChip.setAttribute('aria-pressed', String(contextEnabled !== false))
}

function updateSend() {
  const stop = status.busy && !input.value.trim()
  sendButton.textContent = stop ? 'Stop' : 'Send'
  sendButton.classList.toggle('flareite-primary', !stop)
}

// --- Sending ---

function submit() {
  const text = input.value
  if (!text.trim()) {
    if (status.busy) send({ type: 'interrupt' })
    return
  }
  send({
    type: 'send',
    text,
    context: contextEnabled !== false && !!status.context.label,
    now: Date.now(),
  })
  input.value = ''
  closePopup()
  saveDraft()
  updateSend()
  scrollToEnd()
}

function insertText(text) {
  const start = input.selectionStart ?? input.value.length
  const end = input.selectionEnd ?? start
  const before = input.value.slice(0, start)
  const spacer = before && !/\s$/.test(before) ? ' ' : ''
  input.value = before + spacer + text + input.value.slice(end)
  const at = before.length + spacer.length + text.length
  input.setSelectionRange(at, at)
  input.focus()
  saveDraft()
  updateSend()
}

function saveDraft() {
  flareite.setState({ draft: input.value })
}

// --- Completing @files and /commands ---

function closePopup() {
  completion = null
  popup.hidden = true
  popup.replaceChildren()
}

function showCompletion() {
  const at = input.selectionStart
  const before = input.value.slice(0, at)
  const mention = /(^|\s)@([^\s@]*)$/.exec(before)
  const command = /^\/(\S*)$/.exec(before)
  if (mention) {
    if (files === null) {
      files = []
      send({ type: 'files' })
    }
    const query = mention[2].toLowerCase()
    const scored = []
    for (const file of files) {
      const lower = file.toLowerCase()
      const name = lower.slice(lower.lastIndexOf('/') + 1)
      const score = name.startsWith(query)
        ? 0
        : name.includes(query)
          ? 1
          : lower.includes(query)
            ? 2
            : -1
      if (score >= 0) scored.push([score, file.length, file])
      if (scored.length > 2000) break
    }
    scored.sort((a, b) => a[0] - b[0] || a[1] - b[1])
    const items = scored
      .slice(0, 50)
      .map(([, , file]) => ({ label: file, insert: '@' + file + ' ', hint: '' }))
    open('file', at - mention[2].length - 1, items)
  } else if (command && status.commands.length) {
    const query = command[1].toLowerCase()
    const items = status.commands
      .filter(entry => entry.name.toLowerCase().includes(query))
      .sort((a, b) => Number(!a.name.startsWith(query)) - Number(!b.name.startsWith(query)))
      .slice(0, 50)
      .map(entry => ({
        label: '/' + entry.name,
        insert: '/' + entry.name + ' ',
        hint: entry.hint ? entry.hint + ' — ' + entry.description : entry.description,
      }))
    open('command', 0, items)
  } else {
    closePopup()
  }
}

function open(kind, start, items) {
  if (!items.length) return closePopup()
  completion = { kind, start, items, index: 0 }
  popup.replaceChildren()
  items.forEach((item, index) => {
    const row = el('div', 'popup__item')
    row.setAttribute('role', 'option')
    row.append(el('span', '', item.label))
    if (item.hint) row.append(el('small', '', item.hint))
    row.addEventListener('mousedown', event => {
      event.preventDefault()
      choose(index)
    })
    popup.append(row)
  })
  popup.hidden = false
  highlight()
}

function highlight() {
  ;[...popup.children].forEach((row, index) => {
    const selected = index === completion.index
    row.setAttribute('aria-selected', String(selected))
    if (selected) row.scrollIntoView({ block: 'nearest' })
  })
}

function choose(index) {
  const { start, items } = completion
  const at = input.selectionStart
  input.value = input.value.slice(0, start) + items[index].insert + input.value.slice(at)
  const caret = start + items[index].insert.length
  input.setSelectionRange(caret, caret)
  closePopup()
  saveDraft()
  input.focus()
}

// --- History ---

function showHistory(sessions) {
  const list = $('historyList')
  list.replaceChildren()
  if (!sessions.length) list.append(el('li', 'muted', 'No past chats in this folder.'))
  for (const session of sessions) {
    const item = el('li')
    const choice = button('', () => {
      $('historyPanel').hidden = true
      send({ type: 'resume', id: session.id })
    })
    choice.className = ''
    choice.append(el('span', 'history__title', session.title || 'Untitled chat'))
    if (session.updated) choice.append(el('span', 'muted', when(session.updated)))
    item.append(choice)
    list.append(item)
  }
  $('historyPanel').hidden = false
}

function when(time) {
  const minutes = Math.round((Date.now() - time) / 60000)
  if (minutes < 1) return 'now'
  if (minutes < 60) return minutes + ' min ago'
  if (minutes < 60 * 24) return Math.round(minutes / 60) + ' h ago'
  return new Date(time).toLocaleDateString()
}

function toast(text) {
  const node = $('toast')
  node.textContent = text
  node.hidden = false
  clearTimeout(toast.timer)
  toast.timer = setTimeout(() => (node.hidden = true), 3000)
}

// --- Messages from the extension ---

flareite.onMessage(message => {
  switch (message.type) {
    case 'reset':
      loaded = true
      reset()
      setStatus(message.status)
      break
    case 'items':
      if (loaded) message.items.forEach(put)
      if (loaded) scrollToEnd()
      break
    case 'item':
      if (loaded) put(message.item)
      break
    case 'status':
      if (loaded) setStatus(message)
      break
    case 'context':
      setContext(message.context)
      break
    case 'insert':
      insertText(message.text)
      break
    case 'files':
      files = message.files
      if (completion?.kind === 'file' || document.activeElement === input) showCompletion()
      break
    case 'history':
      showHistory(message.sessions)
      break
    case 'toast':
      toast(message.text)
      break
  }
})

// --- What the user does ---

sendButton.addEventListener('click', submit)
$('new').addEventListener('click', () => send({ type: 'newChat' }))
$('history').addEventListener('click', () => send({ type: 'history' }))
$('historyClose').addEventListener('click', () => ($('historyPanel').hidden = true))
$('problems').addEventListener('click', () => send({ type: 'problems' }))
$('model').addEventListener('change', event => send({ type: 'model', value: event.target.value }))
$('mode').addEventListener('change', event => send({ type: 'mode', value: event.target.value }))
$('effort').addEventListener('change', event => send({ type: 'effort', value: event.target.value }))
contextChip.addEventListener('click', () => {
  contextEnabled = contextEnabled === false
  setContext(status.context)
})

input.addEventListener('input', () => {
  saveDraft()
  updateSend()
  showCompletion()
})
input.addEventListener('blur', () => setTimeout(closePopup, 150))
input.addEventListener('keydown', event => {
  if (event.isComposing) return
  if (completion) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      const count = completion.items.length
      completion.index = (completion.index + (event.key === 'ArrowDown' ? 1 : count - 1)) % count
      highlight()
      return
    }
    if (event.key === 'Enter' || event.key === 'Tab') {
      event.preventDefault()
      choose(completion.index)
      return
    }
    if (event.key === 'Escape') {
      event.preventDefault()
      closePopup()
      return
    }
  }
  if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault()
    submit()
  } else if (event.key === 'Escape' && status.busy) {
    event.preventDefault()
    send({ type: 'interrupt' })
  }
})

// Links and file names in Claude's text open through the extension.
transcript.addEventListener('click', event => {
  const link = event.target.closest('a[href]')
  if (link) {
    event.preventDefault()
    const href = link.getAttribute('href')
    if (/^(https?:|mailto:)/i.test(href)) send({ type: 'openLink', url: href })
    else if (href && !href.startsWith('#'))
      send({ type: 'openFile', ...parseFileReference(decodeURI(href)) })
    return
  }
  const code = event.target.closest('code.path')
  if (code) send({ type: 'openFile', ...parseFileReference(code.textContent) })
})

const saved = flareite.getState()
if (saved && typeof saved.draft === 'string') input.value = saved.draft
updateSend()
input.focus()
send({ type: 'ready' })
