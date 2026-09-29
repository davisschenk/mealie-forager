-- 'social' runs the yt-dlp/transcribe/extract pipeline; 'web' lets Mealie scrape the page.
ALTER TABLE jobs ADD COLUMN source TEXT NOT NULL DEFAULT 'social';
