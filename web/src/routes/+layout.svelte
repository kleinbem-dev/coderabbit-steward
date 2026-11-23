<script lang="ts">
	import '../app.css';
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';

	let { children }: { children?: Snippet } = $props();
</script>

<div class="min-h-screen bg-[var(--bg)] text-[var(--fg)]">
	<header class="border-b border-[var(--border)] px-6 py-3 flex items-center justify-between">
		<div class="flex items-center gap-6">
			<a href="/" class="font-semibold tracking-tight text-base">coderabbit-steward</a>
			<nav class="flex items-center gap-4 text-sm text-[var(--fg-muted)]">
				<a href="/" class:font-medium={page.url.pathname === '/'}>Fleet</a>
				<a href="/findings" class:font-medium={page.url.pathname === '/findings'}>Findings</a>
				<a href="/settings" class:font-medium={page.url.pathname === '/settings'}>Settings</a>
			</nav>
		</div>
		<span class="text-xs text-[var(--fg-muted)]">Europe/Dublin</span>
	</header>

	<main class="max-w-6xl mx-auto px-6 py-6">
		{#if children}
			{@render children()}
		{/if}
	</main>
</div>
