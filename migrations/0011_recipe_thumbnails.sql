-- Dish-photo thumbnails for social-video imports. During extraction the local
-- ffmpeg pipeline picks up to four cropped food-shot candidates; they ride
-- with the expiring draft so the preview page can offer a choice, and the
-- chosen JPEG is copied onto the recipe when the draft is applied.
ALTER TABLE recipes ADD COLUMN thumbnail_jpeg BLOB;

CREATE TABLE draft_thumbnails (
    draft_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    seconds INTEGER NOT NULL,
    jpeg BLOB NOT NULL,
    PRIMARY KEY (draft_id, idx)
);
