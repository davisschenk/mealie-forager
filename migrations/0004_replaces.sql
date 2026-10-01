-- A re-import deletes this Mealie recipe once the new one is imported and cleaned.
ALTER TABLE jobs ADD COLUMN replaces_slug TEXT;
