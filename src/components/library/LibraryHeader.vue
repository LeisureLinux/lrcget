<template>
  <div
    id="library-header"
    class="px-4 py-2 h-12 flex justify-between gap-4 flex-none items-stretch"
  >
    <!-- The header row must never push the menu button off-screen. The search
         field is the only flexible part; everything else is fixed width. -->
    <div class="flex-1 ml-2 min-w-0">
      <MiniSearch :active-tab="props.activeTab" />
    </div>

    <div class="flex-1 flex gap-4 justify-center items-center text-sm">
      <button
        class="tab"
        :class="{
          'active-tab': props.activeTab === 'tracks',
          'inactive-tab': activeTab !== 'tracks',
        }"
        @click.prevent="$emit('changeActiveTab', 'tracks')"
      >
        {{ $t('tabs.tracks') }}
      </button>
      <button
        class="tab"
        :class="{
          'active-tab': props.activeTab === 'albums',
          'inactive-tab': activeTab !== 'albums',
        }"
        @click.prevent="$emit('changeActiveTab', 'albums')"
      >
        {{ $t('tabs.albums') }}
      </button>
      <button
        class="tab"
        :class="{
          'active-tab': props.activeTab === 'artists',
          'inactive-tab': activeTab !== 'artists',
        }"
        @click.prevent="$emit('changeActiveTab', 'artists')"
      >
        {{ $t('tabs.artists') }}
      </button>

      <!-- Create a separator -->
      <div class="w-[2px] h-[70%] bg-neutral-700/50" />

      <button
        class="tab"
        :class="{
          'active-tab': props.activeTab === 'my-lrclib',
          'inactive-tab': activeTab !== 'my-lrclib',
        }"
        @click.prevent="$emit('changeActiveTab', 'my-lrclib')"
      >
        {{ $t('tabs.lrclib') }}
      </button>
    </div>

    <div class="flex-1 flex justify-end items-center gap-1">
      <button
        v-if="isBuildingQueue"
        class="button button-disabled px-4 py-1.5 h-full min-w-[12rem] text-xs rounded-full"
        disabled
        @click.prevent="$emit('showDownloadViewer')"
      >
        <div class="animate-spin text-sm">
          <Loading />
        </div>
        <div class="flex gap-1">
          <div>{{ $t('status.preparing') }}</div>
        </div>
      </button>

      <button
        v-else-if="isDownloading && downloadedCount !== downloadTotalCount"
        class="button button-working h-full min-w-[12rem] px-2 text-xs rounded-full"
        @click.prevent="$emit('showDownloadViewer')"
      >
        <div class="animate-spin text-sm">
          <Loading />
        </div>
        <div class="flex gap-1">
          <div>{{ $t('status.downloading') }}</div>
          <div>{{ downloadedCount }}/{{ downloadTotalCount }}</div>
          <div v-if="downloadFailureCount > 0">
            ({{ downloadFailureCount }} {{ $t('viewer.notFound') }})
          </div>
        </div>
      </button>

      <button
        v-else-if="isDownloading"
        class="button button-done h-full min-w-[12rem] px-2 text-xs rounded-full"
        @click.prevent="$emit('showDownloadViewer')"
      >
        <div class="text-sm">
          <Check />
        </div>
        <span> {{ $t('status.downloadDone') }} {{ downloadedCount }}/{{ downloadTotalCount }} </span>
        <span v-if="downloadFailureCount > 0" class="text-yellow-600">
          ({{ downloadFailureCount }} {{ $t('viewer.notFound') }})
        </span>
      </button>

      <button
        v-else
        class="button button-primary h-full min-w-[12rem] px-2 text-xs rounded-full"
        @click.stop.prevent="openDownloadOptions"
      >
        <DownloadMultiple />
        <span>{{ $t('actions.downloadAll') }}</span>
      </button>

      <!-- Fetching a track from YouTube. Separate from the lyric download,
           because this one runs an external program and needs a cookie
           export the user has to provide. -->
      <button
        class="button button-normal h-full min-w-[6rem] px-2 text-xs rounded-full"
        :title="$t('youtube.title')"
        @click.stop.prevent="$emit('showYoutubeDownload')"
      >
        <Youtube />
        <span>{{ $t('youtube.button') }}</span>
      </button>

      <button
        v-if="isExporting && exportedCount + skippedCount + errorCount < exportTotalCount"
        class="button button-working h-full min-w-[7rem] px-2 text-xs rounded-full"
        @click.prevent="$emit('showExportViewer')"
      >
        <div class="animate-spin text-sm">
          <Loading />
        </div>
        <span>{{ $t('status.exporting') }}</span>
      </button>

      <button
        v-else-if="isExporting"
        class="button button-done h-full min-w-[7rem] px-2 text-xs rounded-full"
        @click.prevent="$emit('showExportViewer')"
      >
        <div class="text-sm">
          <Check />
        </div>
        <span>{{ $t('status.exported') }}</span>
      </button>

      <VDropdown
        v-else
        ref="exportDropdown"
        theme="singalong-dropdown"
        placement="bottom-end"
        class="h-full aspect-square"
        @show="openExportOptions"
      >
        <button
          class="button button-normal h-full min-w-[6rem] px-2 text-xs rounded-full"
          :title="$t('actions.downloadAll')"
        >
          <Export />
          <span>{{ $t('actions.export') }}</span>
        </button>
        <template #popper>
          <div
            class="dropdown-container export-options min-w-[17rem]"
            @keydown.esc="exportDropdown?.hide()"
          >
            <div class="dropdown-section-label">{{ $t('exportOptions.toTrackDirectory') }}</div>

            <fieldset :disabled="exportLoading || submittingExport">
              <label class="dropdown-item">
                <CheckboxButton
                  id="export-plain-text"
                  v-model="exportDraft.plainText"
                  name="export-plain-text"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.plainLyrics') }}</span>
                </CheckboxButton>
              </label>
              <label class="dropdown-item">
                <CheckboxButton
                  id="export-synced-lrc"
                  v-model="exportDraft.syncedLrc"
                  name="export-synced-lrc"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.syncedLyrics') }}</span>
                </CheckboxButton>
              </label>

              <div class="dropdown-section-label mt-2">
                {{ $t('exportOptions.wordLevelHint') }}
              </div>
              <label class="dropdown-item">
                <CheckboxButton
                  id="export-word-none"
                  :model-value="exportDraft.wordFormat === 'none'"
                  name="export-word-format"
                  @update:model-value="setWordFormat('none')"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.wordNone') }}</span>
                </CheckboxButton>
              </label>
              <label class="dropdown-item">
                <CheckboxButton
                  id="export-word-a2"
                  :model-value="exportDraft.wordFormat === 'a2'"
                  name="export-word-format"
                  @update:model-value="setWordFormat('a2')"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.a2') }}</span>
                </CheckboxButton>
              </label>
              <label class="dropdown-item">
                <CheckboxButton
                  id="export-word-ttml"
                  :model-value="exportDraft.wordFormat === 'ttml'"
                  name="export-word-format"
                  @update:model-value="setWordFormat('ttml')"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.ttml') }}</span>
                </CheckboxButton>
              </label>

              <label class="dropdown-item" :class="{ 'cursor-not-allowed': !tryEmbedLyrics }">
                <CheckboxButton
                  id="embed-into-track"
                  v-model="exportDraft.embedIntoTrack"
                  name="embed-into-track"
                  :disabled="!tryEmbedLyrics"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.embedIntoTrack') }}</span>
                </CheckboxButton>
              </label>

              <div class="dropdown-section-label mt-2">
                {{ $t('exportOptions.fileName') }}
              </div>
              <div class="px-2 pb-1">
                <input
                  v-model="exportDraft.template"
                  type="text"
                  class="input w-full text-sm"
                  :placeholder="$t('exportOptions.fileNamePlaceholder')"
                  :disabled="exportLoading || submittingExport"
                  data-testid="export-template-input"
                  @input="templateError = ''"
                />
                <p class="mt-1 text-[11px] leading-snug text-neutral-500 dark:text-neutral-400">
                  {{ $t('exportOptions.fileNameHint') }}
                </p>
                <p v-if="templateError" role="alert" class="options-error">
                  {{ templateError }}
                </p>
              </div>

              <label class="dropdown-item">
                <CheckboxButton
                  id="export-dry-run"
                  v-model="exportDraft.dryRun"
                  name="export-dry-run"
                  :disabled="exportLoading || submittingExport"
                >
                  <span class="dropdown-label">{{ $t('exportOptions.dryRun') }}</span>
                </CheckboxButton>
              </label>
              <p class="px-2 pb-1 text-[11px] leading-snug text-neutral-500 dark:text-neutral-400">
                {{ $t('exportOptions.dryRunHint') }}
              </p>
            </fieldset>
            <p v-if="exportError" role="alert" class="options-error">
              {{ exportError }}
            </p>
            <p v-if="planError" role="alert" class="options-error">
              {{ planError }}
            </p>
            <p v-if="planSummary" class="px-2 text-[11px] leading-snug text-neutral-600 dark:text-neutral-300">
              {{ planSummary }}
            </p>
            <div class="px-2 py-2">
              <button
                class="button w-full text-sm h-8 rounded"
                :class="canExport ? 'button-primary' : 'button-disabled'"
                :disabled="!canExport"
                type="button"
                @click="handleExportClick"
              >
                {{ exportDraft.dryRun ? $t('exportOptions.preview') : $t('actions.export') }}
              </button>
            </div>
          </div>
        </template>
      </VDropdown>

      <VDropdown theme="singalong-dropdown" placement="top-end" class="h-full aspect-square">
        <button class="button button-normal h-full aspect-square rounded-full">
          <DotsVertical />
        </button>
        <template #popper>
          <div class="dropdown-container">
            <button v-close-popper class="dropdown-item" @click="$emit('refreshLibrary')">
              <Refresh class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('actions.refresh')
              }}</span>
            </button>
            <button v-close-popper class="dropdown-item" @click="$emit('manageDirectories')">
              <FolderMultiple class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('config.manageDirectories')
              }}</span>
            </button>
            <button v-close-popper class="dropdown-item" @click="$emit('showConfig')">
              <Cog class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('config.title')
              }}</span>
            </button>
            <button v-close-popper class="dropdown-item" @click="$emit('showRepair')">
              <PlaylistEdit class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('repair.title')
              }}</span>
            </button>
            <button v-close-popper class="dropdown-item" @click="$emit('showLibraryProblems')">
              <PlaylistRemove class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('libraryProblems.title')
              }}</span>
            </button>
            <button v-close-popper class="dropdown-item" @click="$emit('showAbout')">
              <Information class="text-neutral-800 dark:text-neutral-300" />
              <span class="text-neutral-800 dark:text-neutral-300 text-sm font-bold">{{
                $t('about.title')
              }}</span>
            </button>
          </div>
        </template>
      </VDropdown>
    </div>
  </div>
</template>

<script setup>
import { ref, computed } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import DownloadMultiple from '~icons/mdi/download-multiple'
import Loading from '~icons/mdi/loading'
import Check from '~icons/mdi/check'
import Cog from '~icons/mdi/cog'
import Information from '~icons/mdi/information'
import DotsVertical from '~icons/mdi/dots-vertical'
import Refresh from '~icons/mdi/refresh'
import FolderMultiple from '~icons/mdi/folder-multiple'
import Export from '~icons/mdi/export'
import Youtube from '~icons/mdi/youtube'
import PlaylistRemove from '~icons/mdi/playlist-remove'
import PlaylistEdit from '~icons/mdi/playlist-edit'
import CheckboxButton from '@/components/common/CheckboxButton.vue'
import { useDownloader } from '@/composables/downloader.js'
import { useExporter } from '@/composables/export.js'
import { useExportPreferences } from '@/composables/export-preferences.js'
import MiniSearch from './MiniSearch.vue'
import { useDownloadOptions, useDownloadTrigger } from '@/composables/download-options.js'

const props = defineProps(['activeTab'])
const emit = defineEmits([
  'changeActiveTab',
  'showConfig',
  'showAbout',
  'showDownloadViewer',
  'refreshLibrary',
  'manageDirectories',
  'exportAllLyrics',
  'showExportViewer',
  'showYoutubeDownload',
  'showLibraryProblems',
  'showRepair',
])

const { refresh: refreshPreferences, save: savePreferences } = useExportPreferences()
const exportDropdown = ref(null)
// wordFormat is a single choice, not a set: the two word-level formats target
// different players, and writing both would leave a stray file next to every
// track. 'none' keeps the previous behaviour of line-level export only.
const exportDraft = ref({
  plainText: false,
  syncedLrc: true,
  embedIntoTrack: false,
  wordFormat: 'none',
  template: '',
  dryRun: false,
})
const templateError = ref('')
const savedTemplate = ref('')
const planError = ref('')
const planSummary = ref('')

const setWordFormat = value => {
  exportDraft.value.wordFormat = value
}
const tryEmbedLyrics = ref(false)
const exportLoading = ref(false)
const submittingExport = ref(false)
const exportError = ref('')

const { isBuildingQueue } = useDownloadOptions()

const openDownloadOptions = useDownloadTrigger(() => ({ type: 'library' }))

const openExportOptions = async () => {
  exportLoading.value = true
  exportError.value = ''
  planError.value = ''
  planSummary.value = ''
  try {
    const config = await refreshPreferences()
    exportDraft.value = {
      plainText: config.export_txt,
      syncedLrc: config.export_lrc,
      embedIntoTrack: config.export_embedded,
      // Format choice is per-batch, not persisted: which player you are
      // preparing files for is a property of this export run.
      wordFormat: exportDraft.value.wordFormat,
      template: config.export_template || '',
      dryRun: exportDraft.value.dryRun,
    }
    tryEmbedLyrics.value = config.try_embed_lyrics
    savedTemplate.value = config.export_template || ''
  } catch (error) {
    exportError.value = String(error)
  } finally {
    exportLoading.value = false
  }
}

// The template is saved with the other export preferences, but it is validated
// on its own: an unknown variable is a typing mistake, and the backend rejects
// it before anything is written. Showing it here rather than after a batch has
// already run is the difference between one error and fifty.
const submitTemplate = async () => {
  const raw = exportDraft.value.template.trim()
  if (raw === savedTemplate.value) {
    templateError.value = ''
    return
  }
  try {
    await savePreferences({ exportTemplate: raw })
    savedTemplate.value = raw
    templateError.value = ''
  } catch (error) {
    templateError.value = String(error)
    // Put the rejected text back so the user can see what was wrong rather
    // than watching the field change under them.
    exportDraft.value.template = savedTemplate.value
    throw error
  }
}

const canExport = computed(
  () =>
    !exportLoading.value &&
    !submittingExport.value &&
    !exportError.value &&
    (exportDraft.value.plainText ||
      exportDraft.value.syncedLrc ||
      exportDraft.value.wordFormat !== 'none' ||
      (exportDraft.value.embedIntoTrack && tryEmbedLyrics.value))
)

const handleExportClick = async () => {
  if (!canExport.value) return
  submittingExport.value = true
  const draft = { ...exportDraft.value }
  const embedEnabled = tryEmbedLyrics.value
  try {
    await savePreferences({
      exportTxt: draft.plainText,
      exportLrc: draft.syncedLrc,
      ...(embedEnabled ? { exportEmbedded: draft.embedIntoTrack } : {}),
    })
    // The template is saved separately so a rejected one leaves the popup open
    // on its own error instead of failing the whole submission.
    await submitTemplate()

    if (draft.dryRun) {
      // A preview answers "what would this do?" without starting the batch, so
      // the popup stays open and the answer is read in place. The real export
      // hides the popup and reports through the viewer.
      await previewExport(draft)
      return
    }

    exportDropdown.value?.hide()
    emit('exportAllLyrics', {
      ...draft,
      embedIntoTrack: draft.embedIntoTrack && embedEnabled,
      wordFormat: draft.wordFormat,
    })
  } catch (error) {
    if (!templateError.value) exportError.value = String(error)
  } finally {
    submittingExport.value = false
  }
}

// Resolve the export for one track and report what it would do.
//
// A batch is destructive in a way it cannot undo — each sidecar write removes
// the opposite extension first — so the preview is the cheapest place to find
// out that a template points at the wrong folder, or that nothing has timed
// lyrics. One track is enough to answer both; the count is not the question.
const previewExport = async draft => {
  planError.value = ''
  planSummary.value = ''
  const formats = [
    ...(draft.plainText ? ['txt'] : []),
    ...(draft.syncedLrc ? ['lrc'] : []),
    ...(draft.wordFormat === 'a2' ? ['lrcA2'] : []),
    ...(draft.wordFormat === 'ttml' ? ['ttml'] : []),
    ...(draft.embedIntoTrack && embedEnabled ? ['embedded'] : []),
  ]
  if (formats.length === 0) {
    planError.value = 'Select at least one export format'
    return
  }
  try {
    const ids = await invoke('get_track_ids_with_lyrics')
    const trackId = Array.isArray(ids) ? ids[0] : undefined
    if (trackId === undefined) {
      planSummary.value = 'No track in this library has lyrics to export.'
      return
    }
    const plans = await invoke('plan_export_lyrics', { trackId, formats })
    const writes = plans.filter(plan => plan.path)
    const skips = plans.filter(plan => !plan.path)
    planSummary.value = [
      writes.length > 0 ? writes[0].action : null,
      skips.length > 0 ? `skipped: ${skips[0].action}` : null,
    ]
      .filter(Boolean)
      .join('; ')
  } catch (error) {
    planError.value = String(error)
  }
}

const {
  isDownloading,
  totalCount: downloadTotalCount,
  downloadedCount,
  failureCount: downloadFailureCount,
} = useDownloader()

const {
  isExporting,
  exportedCount,
  skippedCount,
  errorCount,
  totalCount: exportTotalCount,
} = useExporter()
</script>

<style scoped>
.active-tab {
  @apply text-neutral-900 border-neutral-900 dark:text-white dark:border-neutral-300;
}

.inactive-tab {
  @apply text-neutral-700/50 hover:text-neutral-700/80 border-transparent dark:text-white/50 dark:hover:text-white/80;
}

.tab {
  @apply transition font-extrabold border-b-2 outline-none py-1;
}

.dropdown-container {
  @apply p-1 min-w-[10rem];
}

.export-options {
  @apply max-w-[calc(100vw-1rem)] max-h-[calc(100vh-4rem)] overflow-y-auto text-neutral-900 dark:text-neutral-100;
}

.options-error {
  @apply px-2 py-1 text-xs leading-relaxed break-words text-red-700 dark:text-red-400;
}

.export-options .button-primary {
  @apply bg-hoa-1400 hover:bg-hoa-1500 active:bg-hoa-1500;
}

.export-options .button-disabled {
  @apply text-neutral-500 dark:text-neutral-400;
}

.dropdown-item {
  @apply flex items-center px-2 py-1 hover:bg-neutral-100 dark:hover:bg-neutral-700 rounded cursor-pointer h-8 gap-1 w-full;
}

.dropdown-label {
  @apply text-neutral-800 dark:text-neutral-300 text-sm font-bold;
}

.dropdown-section-label {
  @apply text-xs uppercase font-bold text-neutral-900 dark:text-neutral-400 px-2 py-1;
}
</style>
