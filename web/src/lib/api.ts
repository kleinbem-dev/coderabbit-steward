export interface FleetRepoBudget {
	repo: string;
	owner: string;
	name: string;
	stars: number;
	free: boolean;
	remaining: number;
	limit: number;
	refill_minutes: number;
	next_free_at: string | null;
	confidence: string;
	queue_length: number;
	coverage_pct: number;
	open_findings: number;
}

export interface SpendRow {
	id: number;
	repo: string;
	identity: string;
	at: string;
	cause: string;
	pr: number | null;
	check_run_id: number | null;
	state: string;
}

export interface QueueRow {
	id: number;
	repo: string;
	kind: string;
	pr_number: number | null;
	priority: number;
	state: string;
	created_at: string;
}

export interface FindingRow {
	id: number;
	repo: string;
	path: string;
	line: number | null;
	severity: string;
	category: string;
	title: string;
	body: string;
	status: string;
	created_at: string;
}

export interface RepoDetail {
	repo: string;
	stars: number;
	budget: {
		free: boolean;
		remaining: number;
		limit: number;
		refill_minutes: number;
		next_free_at: string | null;
		fleet_cooldown_until: string | null;
		confidence: string;
	};
	spends: SpendRow[];
	queue: QueueRow[];
	findings: FindingRow[];
}

export interface Settings {
	mode: string;
	repo_daily_cap: number;
	fleet_daily_cap: number;
	quiet_minutes: number;
	pooled: number;
	updated_at: string;
}

export async function apiFetch<T>(path: string, init?: RequestInit): Promise<T> {
	let token: string | null = null;
	try {
		token = typeof localStorage !== 'undefined' ? localStorage.getItem('cr_steward_token') : null;
	} catch {
		token = null;
	}
	const headers = new Headers(init?.headers);
	if (token) {
		headers.set('Authorization', `Bearer ${token}`);
	}
	const res = await fetch(path, { ...init, headers });
	if (!res.ok) {
		throw new Error(`HTTP ${res.status}: ${await res.text()}`);
	}
	if (res.status === 204) {
		return undefined as T;
	}
	return res.json() as Promise<T>;
}

export function formatIrishTime(iso: string | null): string {
	if (!iso) return '—';
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	return new Intl.DateTimeFormat('en-IE', {
		timeZone: 'Europe/Dublin',
		month: 'short',
		day: '2-digit',
		hour: '2-digit',
		minute: '2-digit',
		hour12: false
	}).format(d);
}
