<script setup lang="ts">
const props = defineProps<{
  timestamp: number;
}>();

const formatDivider = (ts: number): string => {
  const date = new Date(ts);
  const now = new Date();
  const isToday = date.toDateString() === now.toDateString();
  
  const yesterday = new Date(now);
  yesterday.setDate(yesterday.getDate() - 1);
  const isYesterday = date.toDateString() === yesterday.toDateString();
  
  const timeStr = date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  
  if (isToday) return timeStr;
  if (isYesterday) return `Yesterday ${timeStr}`;
  
  return date.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + timeStr;
};
</script>

<template>
  <div class="flex items-center justify-center py-3">
    <span class="text-[11px] th-text-muted th-active-bg px-2.5 py-1 rounded-full">
      {{ formatDivider(timestamp) }}
    </span>
  </div>
</template>
