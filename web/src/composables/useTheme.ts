import { computed, ref, watch } from 'vue'
import { readJSON, readString, storageKeys, writeJSON } from '@/lib/storage'

const STORAGE_KEY = storageKeys.theme
const LEGACY_KEY = 'gasket_theme'

export type ThemeMode = 'light' | 'dark'
export type ThemeHue = 'zinc' | 'blue' | 'rose' | 'emerald' | 'amber' | 'violet'
export type MarkdownStyle = 'classic' | 'github'

export interface ThemeState {
  mode: ThemeMode
  hue: ThemeHue
  markdownStyle: MarkdownStyle
}

const HUES: ThemeHue[] = ['zinc', 'blue', 'rose', 'emerald', 'amber', 'violet']
const MARKDOWN_STYLES: MarkdownStyle[] = ['classic', 'github']

// Migrate legacy/removed markdown style names to the current set
function migrateMarkdownStyle(old: string | undefined): MarkdownStyle {
  return old === 'github' ? 'github' : 'classic'
}

function getInitialState(): ThemeState {
  // Try new format first
  const parsed = readJSON<Partial<ThemeState> | null>(STORAGE_KEY, null)
  if (parsed && parsed.mode && parsed.hue && HUES.includes(parsed.hue)) {
    const md: MarkdownStyle = migrateMarkdownStyle(parsed.markdownStyle)
    return { mode: parsed.mode, hue: parsed.hue, markdownStyle: md }
  }

  // Migrate from legacy single-value theme
  const legacy = readString(LEGACY_KEY) as ThemeMode | ''
  if (legacy === 'light' || legacy === 'dark') {
    return { mode: legacy, hue: 'zinc', markdownStyle: 'classic' }
  }

  // System preference
  const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches
  return { mode: prefersDark ? 'dark' : 'light', hue: 'zinc', markdownStyle: 'classic' }
}

// Module-level singleton state so all components share the same theme
const _state = ref<ThemeState>(getInitialState())

const applyTheme = (s: ThemeState) => {
  const root = document.documentElement
  if (s.mode === 'light') {
    root.classList.remove('dark')
  } else {
    root.classList.add('dark')
  }
  root.setAttribute('data-hue', s.hue)
  root.setAttribute('data-md-style', s.markdownStyle)
  writeJSON(STORAGE_KEY, s)
}

applyTheme(_state.value)

watch(_state, (s) => {
  applyTheme(s)
}, { deep: true })

export function useTheme() {
  const setMode = (mode: ThemeMode) => {
    _state.value.mode = mode
  }

  const setHue = (hue: ThemeHue) => {
    _state.value.hue = hue
  }

  const setMarkdownStyle = (style: MarkdownStyle) => {
    _state.value.markdownStyle = style
  }

  const cycleMode = () => {
    _state.value.mode = _state.value.mode === 'light' ? 'dark' : 'light'
  }

  return {
    mode: computed(() => _state.value.mode),
    hue: computed(() => _state.value.hue),
    markdownStyle: computed(() => _state.value.markdownStyle),
    state: _state,
    setMode,
    setHue,
    setMarkdownStyle,
    cycleMode,
    hues: HUES,
    markdownStyles: MARKDOWN_STYLES,
  }
}
