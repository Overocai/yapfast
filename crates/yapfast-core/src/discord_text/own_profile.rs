//! This account's own profile, in a Discord server or its main one:
//! fetching it for its editor, reading a picture chosen for it as Discord's
//! client sends one, and saving it. See "Editing one's profile" in
//! docs/discord/people.md.

use std::path::Path;

use base64::Engine;
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use serde_json::{Map, Value};
use yapfast_discord::profile::PICTURE_LIMIT;

use super::{commands::NOT_DISCORD, *};
use crate::chat::{
    OwnProfile, PickedPicture, PictureCrop, Pixels, ProfileChange, ProfilePicture, Wishlist,
    WishlistItem, file_size,
};

/// The size Discord's client crops a picture to before it sends it: a
/// square avatar, and a banner 17 by 6. One smaller goes at its own size.
const AVATAR: (u32, u32) = (1024, 1024);
const BANNER: (u32, u32) = (2400, 848);
/// The preview's size: as the editor's card gets everyone's pictures, a
/// 128px avatar for its 120px one, and a banner 600 wide.
const AVATAR_PREVIEW: (u32, u32) = (128, 128);
const BANNER_PREVIEW: (u32, u32) = (600, 212);

/// Carries out a command about this account's own profile, or gives back
/// any other.
pub(super) fn command(
    command: Command,
    discord: Result<&Discord, &str>,
    events: &UnboundedSender<Event>,
) -> Option<Command> {
    let Command::Discord(command) = command else {
        return Some(command);
    };
    let events = events.clone();
    match command {
        DiscordCommand::OwnProfile { server } => {
            let guild = guild(server.as_deref());
            let (Ok(discord), Some(guild)) = (discord, guild) else {
                let why = discord.err().unwrap_or(NOT_DISCORD);
                let _ = events.send(Event::OwnProfile {
                    server,
                    result: Err(why.into()),
                });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let result = match discord.own_profile_and_card(guild.as_deref()).await {
                    Ok((profile, card)) => {
                        // The main profile's answer is the account card's
                        // too, so it needn't be asked for again.
                        let me = discord.snapshots().borrow().user_id.clone();
                        if let (Some(card), Some(user)) = (card, me) {
                            let card = super::render::chat_profile(
                                &discord,
                                yapfast_discord::ready::PRIVATE,
                                card,
                            );
                            let _ = events.send(Event::Profile {
                                user,
                                result: Ok(card),
                            });
                        }
                        Ok(own_profile(&discord, guild.as_deref(), profile))
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = format!("{error:#}"),
                            "could not fetch this account's profile"
                        );
                        Err(format!("Discord didn't send your profile: {error}"))
                    }
                };
                let _ = events.send(Event::OwnProfile { server, result });
            });
        }
        DiscordCommand::Wishlist { id } => {
            let Ok(discord) = discord else {
                let _ = events.send(Event::Wishlist {
                    id,
                    wishlist: Wishlist::Failed,
                });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let wishlist = match discord.wishlist(&id).await {
                    Ok(items) => Wishlist::Loaded(
                        items
                            .into_iter()
                            .map(|item| WishlistItem {
                                name: item.name,
                                picture: item.picture,
                            })
                            .collect(),
                    ),
                    Err(error) => wishlist_refusal(&error),
                };
                let _ = events.send(Event::Wishlist { id, wishlist });
            });
        }
        // Read-only: what the board names comes from `GET /games` and the
        // applications' public objects.
        DiscordCommand::ProfileBoard { user, widgets } => {
            let Ok(discord) = discord else {
                let _ = events.send(Event::ProfileBoard { user, widgets });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let mut asked: Vec<yapfast_discord::widgets::Widget> =
                    widgets.iter().map(asked_widget).collect();
                discord.name_widgets(&mut asked).await;
                let widgets = widgets
                    .into_iter()
                    .zip(asked)
                    .map(|(widget, named)| named_widget(widget, named))
                    .collect();
                let _ = events.send(Event::ProfileBoard { user, widgets });
            });
        }
        DiscordCommand::ProfileRecent { user } => {
            let Ok(discord) = discord else {
                let recent = Vec::new();
                let _ = events.send(Event::ProfileRecent { user, recent });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let recent = match discord.recent_activity(&user).await {
                    Ok(entries) => entries
                        .into_iter()
                        .map(|entry| crate::chat::RecentActivity {
                            title: entry.title,
                            subtitle: entry.subtitle,
                            picture: entry.picture,
                            music: entry.music,
                            at: entry.at,
                        })
                        .collect(),
                    Err(error) => {
                        tracing::debug!(%error, "could not get someone's recent activity");
                        Vec::new()
                    }
                };
                let _ = events.send(Event::ProfileRecent { user, recent });
            });
        }
        // Read-only, and only on a click, as Discord's client asks.
        DiscordCommand::ActivityLinks {
            user,
            session,
            application,
        } => {
            let Ok(discord) = discord else {
                let why = discord.err().unwrap_or(NOT_DISCORD);
                let _ = events.send(Event::ActivityLinks {
                    user,
                    session,
                    application,
                    result: Err(why.into()),
                });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let result = discord
                    .activity_button_urls(&user, &session, &application)
                    .await
                    .map(|urls| {
                        urls.into_vec()
                            .into_iter()
                            .map(|url| url.map(String::from))
                            .collect()
                    })
                    .map_err(|error| error.to_string());
                let _ = events.send(Event::ActivityLinks {
                    user,
                    session,
                    application,
                    result,
                });
            });
        }
        DiscordCommand::ProfilePicture { kind, path, crop } => {
            tokio::task::spawn_blocking(move || {
                let result = read_picture(&path, kind, crop);
                let _ = events.send(Event::ProfilePicture { kind, path, result });
            });
        }
        DiscordCommand::SaveProfile { server, change } => {
            let guild = guild(server.as_deref());
            let (Ok(discord), Some(guild)) = (discord, guild) else {
                let why = discord.err().unwrap_or(NOT_DISCORD);
                let _ = events.send(Event::ProfileSaved {
                    server,
                    result: Err(why.into()),
                });
                return None;
            };
            let discord = discord.clone();
            tokio::spawn(async move {
                let result = match &guild {
                    Some(guild) => {
                        let (member, profile) = server_bodies(&change);
                        drop(change);
                        discord.save_server_profile(guild, &member, &profile).await
                    }
                    None => {
                        let (user, profile) = main_bodies(&change);
                        let tag = change.primary_server.clone();
                        drop(change);
                        discord
                            .save_main_profile(
                                &user,
                                &profile,
                                tag.as_ref().map(|tag| tag.as_deref()),
                            )
                            .await
                    }
                };
                let result = result.map_err(|error| {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "could not save this account's profile"
                    );
                    match guild {
                        Some(_) => {
                            format!("There was a problem updating your server profile: {error}")
                        }
                        None => format!("There was a problem updating your profile: {error}"),
                    }
                });
                let _ = events.send(Event::ProfileSaved { server, result });
            });
        }
        other => return Some(Command::Discord(other)),
    }
    None
}

/// A widget as the service names it.
fn asked_widget(widget: &crate::chat::ProfileWidget) -> yapfast_discord::widgets::Widget {
    yapfast_discord::widgets::Widget {
        title: widget.title.clone(),
        application: widget.application.clone(),
        lists_games: widget.lists_games,
        games: widget
            .games
            .iter()
            .map(|game| yapfast_discord::widgets::WidgetGame {
                id: game.id.clone(),
                comment: None,
                name: game.name.clone(),
                icon: game.icon.clone(),
            })
            .collect(),
    }
}

/// A widget with the names the service found, and the comments it was asked with.
fn named_widget(
    widget: crate::chat::ProfileWidget,
    named: yapfast_discord::widgets::Widget,
) -> crate::chat::ProfileWidget {
    crate::chat::ProfileWidget {
        title: named.title,
        application: widget.application,
        lists_games: widget.lists_games,
        games: widget
            .games
            .into_iter()
            .zip(named.games)
            .map(|(game, named)| crate::chat::ProfileGame {
                id: game.id,
                name: named.name,
                icon: named.icon,
                comment: game.comment,
            })
            .collect(),
    }
}

/// What Discord refusing a wishlist means: it's hidden from this account when
/// it's forbidden or said not to exist, as a private one is to anyone else.
fn wishlist_refusal(error: &anyhow::Error) -> Wishlist {
    use yapfast_discord::api::Refused;
    match error.downcast_ref::<Refused>() {
        Some(Refused(status)) if matches!(status.as_u16(), 403 | 404) => Wishlist::Hidden,
        _ => {
            tracing::warn!(error = format!("{error:#}"), "could not fetch a wishlist");
            Wishlist::Failed
        }
    }
}

/// The Discord server a profile is in, by its server id: `Some(None)` for
/// the main profile, and none for an id that isn't a server's.
fn guild(server: Option<&str>) -> Option<Option<String>> {
    match server {
        Some(server) => discord::parse_server(server).map(|guild| Some(guild.to_owned())),
        None => Some(None),
    }
}

/// The profile as Yapfast shows it, its fallback's bio's mentions written
/// as names.
fn own_profile(
    discord: &Discord,
    guild: Option<&str>,
    profile: yapfast_discord::profile::OwnProfile,
) -> OwnProfile {
    OwnProfile {
        name: profile.name,
        pronouns: profile.pronouns,
        bio: profile.bio,
        avatar: profile.avatar,
        banner: profile.banner,
        accent: profile.accent,
        fallback: render::chat_profile(discord, guild.unwrap_or_default(), profile.fallback),
    }
}

/// A picture as Discord's client sends it: a data URI, or null to go by
/// the fallback's.
pub(super) fn picture(picture: &Option<PickedPicture>) -> Value {
    match picture {
        Some(picture) => Value::String(format!(
            "data:{};base64,{}",
            picture.mime,
            base64::engine::general_purpose::STANDARD.encode(&picture.data)
        )),
        None => Value::Null,
    }
}

/// What Discord's client sends for a change in a server: the member's
/// fields, its nickname and avatar, for `PATCH /guilds/{id}/members/@me`,
/// and the profile's, its banner, bio and pronouns, for `PATCH
/// /guilds/{id}/profile/@me`. An empty text goes by the main profile's.
fn server_bodies(change: &ProfileChange) -> (Value, Value) {
    let (mut member, mut profile) = (Map::new(), Map::new());
    if let Some(nick) = &change.name {
        member.insert("nick".into(), nick.as_str().into());
    }
    if let Some(avatar) = &change.avatar {
        member.insert("avatar".into(), picture(avatar));
    }
    if let Some(banner) = &change.banner {
        profile.insert("banner".into(), picture(banner));
    }
    if let Some(bio) = &change.bio {
        profile.insert("bio".into(), bio.as_str().into());
    }
    if let Some(pronouns) = &change.pronouns {
        profile.insert("pronouns".into(), pronouns.as_str().into());
    }
    (Value::Object(member), Value::Object(profile))
}

/// What Discord's client sends for a change to the main profile: the
/// user's fields, its display name and avatar, for `PATCH /users/@me`, and
/// the profile's, its bio, pronouns, banner and banner colour, for `PATCH
/// /users/@me/profile`. An empty display name goes by the username. The
/// server tag is neither's: Discord's editor sends it to `PUT
/// /users/@me/clan`; see `Discord::save_main_profile`.
fn main_bodies(change: &ProfileChange) -> (Value, Value) {
    let (mut user, mut profile) = (Map::new(), Map::new());
    if let Some(name) = &change.name {
        user.insert("global_name".into(), name.as_str().into());
    }
    if let Some(avatar) = &change.avatar {
        user.insert("avatar".into(), picture(avatar));
    }
    if let Some(bio) = &change.bio {
        profile.insert("bio".into(), bio.as_str().into());
    }
    if let Some(pronouns) = &change.pronouns {
        profile.insert("pronouns".into(), pronouns.as_str().into());
    }
    if let Some(banner) = &change.banner {
        profile.insert("banner".into(), picture(banner));
    }
    if let Some(accent) = change.accent {
        profile.insert("accent_color".into(), accent.into());
    }
    if let Some(theme) = change.theme {
        profile.insert("theme_colors".into(), serde_json::json!(theme));
    }
    if let Some(style) = &change.name_style {
        user.insert(
            "display_name_font_id".into(),
            serde_json::json!(style.map(|style| style.font)),
        );
        user.insert(
            "display_name_effect_id".into(),
            serde_json::json!(style.map(|style| style.effect)),
        );
        user.insert(
            "display_name_colors".into(),
            serde_json::json!(style.as_ref().map(|style| style.colors())),
        );
    }
    (Value::Object(user), Value::Object(profile))
}

/// The picture at `path` as Discord's client sends it for a profile: cut
/// where `chosen` frames it, or else from its middle, to a square avatar or
/// a 17 by 6 banner, at most [`AVATAR`] or [`BANNER`], with a preview at
/// the editor's size. Only the file to send and the preview stay; the
/// decoded picture goes. An animated GIF stays animated: cut frame by
/// frame where it was framed, or else sent as it is.
pub(super) fn read_picture(
    path: &Path,
    kind: ProfilePicture,
    chosen: Option<PictureCrop>,
) -> Result<PickedPicture, String> {
    let (largest, preview) = match kind {
        ProfilePicture::Avatar => (AVATAR, AVATAR_PREVIEW),
        ProfilePicture::Banner => (BANNER, BANNER_PREVIEW),
    };
    read_shaped(path, largest, preview, "a profile picture", chosen)
}

/// A picture chosen for a profile as the Edit Image window shows it: its
/// own size, and its pixels fitted within [`CROP_SHOWN`].
pub struct CropSource {
    /// The picture's own width and height.
    pub size: (u32, u32),
    /// The width and height it's shown at.
    pub shown: (u32, u32),
    /// Its pixels as shown, straight RGBA.
    pub rgba: Vec<u8>,
}

/// The longest side the Edit Image window shows a picture at: its frame
/// is 260 px across, zoomed up to four times.
const CROP_SHOWN: u32 = 1280;

/// The picture at `path` as the Edit Image window shows it, or why it
/// can't be a profile picture, as reading it for the profile would say.
pub fn read_to_crop(path: &Path) -> Result<CropSource, String> {
    let (_, _, decoded) = open_picture(path, "a profile picture")?;
    let size = decoded.dimensions();
    let shown = if size.0.max(size.1) > CROP_SHOWN {
        decoded.thumbnail(CROP_SHOWN, CROP_SHOWN).into_rgba8()
    } else {
        decoded.into_rgba8()
    };
    Ok(CropSource {
        size,
        shown: shown.dimensions(),
        rgba: shown.into_raw(),
    })
}

/// The picture at `path` read and decoded as one for `what` (such as "a
/// profile picture") would be: its file, its format and its pixels, or why
/// it can't be one.
fn open_picture(path: &Path, what: &str) -> Result<(Vec<u8>, ImageFormat, DynamicImage), String> {
    let name = file_name(path);
    let size = std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .ok_or_else(|| format!("Yapfast could not read {name}."))?
        .len();
    if size > PICTURE_LIMIT {
        return Err(format!(
            "{name} is {}; {what} can be up to {}.",
            file_size(size),
            file_size(PICTURE_LIMIT)
        ));
    }
    let data = std::fs::read(path).map_err(|_| format!("Yapfast could not read {name}."))?;
    let not_a_picture = || format!("{name} isn't a PNG, JPG, GIF or WebP picture.");
    let format = image::guess_format(&data)
        .ok()
        .filter(|format| {
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
            )
        })
        .ok_or_else(not_a_picture)?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(&data), format);
    reader.limits(picture_limits());
    let decoded = reader.decode().map_err(|error| {
        tracing::info!(%error, "could not decode {what}");
        not_a_picture()
    })?;
    Ok((data, format, decoded))
}

/// A file's name, as an error names it.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// How large a picture chosen for a profile may decode: 8192 px each way,
/// and 256 MiB.
fn picture_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits
}

/// The picture at `path` as Discord's client sends it: cut where `chosen`
/// frames it, or else from its middle, in the shape of `largest`, and at
/// most that, with a preview of `preview`'s size. `what` names it in an
/// error, such as "a profile picture".
pub(super) fn read_shaped(
    path: &Path,
    largest: (u32, u32),
    preview: (u32, u32),
    what: &str,
    chosen: Option<PictureCrop>,
) -> Result<PickedPicture, String> {
    let (data, format, decoded) = open_picture(path, what)?;
    let not_a_picture = || format!("{} isn't a PNG, JPG, GIF or WebP picture.", file_name(path));
    let (width, height) = decoded.dimensions();
    let rect = chosen.map(|chosen| chosen.within(width, height, largest));
    let cropped = crop(&decoded, largest, rect);
    drop(decoded);
    let small = cropped.thumbnail_exact(preview.0, preview.1).into_rgba8();
    let preview = Arc::new(Picture {
        width: small.width(),
        height: small.height(),
        rgba: Pixels::new(small.width(), small.height(), small.into_raw()),
        animation: None,
    });
    if format == ImageFormat::Gif {
        // Cut where it was framed, frame by frame; one that can't be, or
        // comes out too large, goes as it is.
        let data: Arc<[u8]> = match rect {
            Some(rect) => match crop_gif(&data, rect, largest) {
                Ok(cut) if cut.len() as u64 <= PICTURE_LIMIT => cut.into(),
                Ok(cut) => {
                    tracing::info!(bytes = cut.len(), "a cut GIF came out too large to send");
                    data.into()
                }
                Err(error) => {
                    tracing::info!(%error, "could not cut a GIF");
                    data.into()
                }
            },
            None => data.into(),
        };
        return Ok(PickedPicture {
            mime: "image/gif",
            data,
            preview,
        });
    }
    drop(data);
    // A picture with transparency stays a PNG; a photo goes as a JPEG,
    // a fraction of the size.
    let mut file = std::io::Cursor::new(Vec::new());
    let mime = if cropped.color().has_alpha() {
        cropped
            .write_to(&mut file, ImageFormat::Png)
            .map_err(|_| not_a_picture())?;
        "image/png"
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 90)
            .encode_image(&cropped.into_rgb8())
            .map_err(|_| not_a_picture())?;
        "image/jpeg"
    };
    Ok(PickedPicture {
        mime,
        data: file.into_inner().into(),
        preview,
    })
}

/// `picture` cut at `rect` (its left and top, width and height), or else
/// its middle in the shape of `largest`, shrunk to `largest` if it's larger.
fn crop(
    picture: &DynamicImage,
    largest: (u32, u32),
    rect: Option<(u32, u32, u32, u32)>,
) -> DynamicImage {
    let (width, height) = picture.dimensions();
    let (x, y, crop_width, crop_height) = rect.unwrap_or_else(|| middle(width, height, largest));
    let cropped = picture.crop_imm(x, y, crop_width, crop_height);
    if crop_width > largest.0 {
        cropped.resize_exact(largest.0, largest.1, FilterType::Lanczos3)
    } else {
        cropped
    }
}

/// The middle of a picture `width` by `height` in the shape of `largest`,
/// the widest part of the shape that fits, as whole pixels: its left and
/// top, width and height.
fn middle(width: u32, height: u32, largest: (u32, u32)) -> (u32, u32, u32, u32) {
    let (shape_width, shape_height) = (u64::from(largest.0.max(1)), u64::from(largest.1.max(1)));
    let (crop_width, crop_height) =
        if u64::from(width) * shape_height > u64::from(height) * shape_width {
            let crop_width = (u64::from(height) * shape_width / shape_height) as u32;
            (crop_width.max(1), height)
        } else {
            let crop_height = (u64::from(width) * shape_height / shape_width) as u32;
            (width, crop_height.max(1))
        };
    (
        (width - crop_width) / 2,
        (height - crop_height) / 2,
        crop_width,
        crop_height,
    )
}

/// A GIF cut at `rect` (its left and top, width and height) frame by
/// frame, each shrunk to `largest` if it's larger, and encoded again to
/// loop as it did. Each frame is decoded, cut and encoded before the next,
/// so only one is held at a time.
fn crop_gif(
    data: &[u8],
    (x, y, width, height): (u32, u32, u32, u32),
    largest: (u32, u32),
) -> image::ImageResult<Vec<u8>> {
    use image::{
        AnimationDecoder, ImageDecoder,
        codecs::gif::{GifDecoder, GifEncoder, Repeat},
    };
    let mut decoder = GifDecoder::new(std::io::Cursor::new(data))?;
    decoder.set_limits(picture_limits())?;
    let size = if width > largest.0 {
        largest
    } else {
        (width, height)
    };
    let mut file = Vec::new();
    {
        let mut encoder = GifEncoder::new_with_speed(&mut file, 10);
        encoder.set_repeat(Repeat::Infinite)?;
        for frame in decoder.into_frames() {
            let frame = frame?;
            let delay = frame.delay();
            let canvas = frame.into_buffer();
            let mut cut = image::imageops::crop_imm(&canvas, x, y, width, height).to_image();
            drop(canvas);
            if size != (width, height) {
                cut = image::imageops::resize(&cut, size.0, size.1, FilterType::Triangle);
            }
            encoder.encode_frame(image::Frame::from_parts(cut, 0, 0, delay))?;
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `width` by `height` picture saved as `format` in a new directory.
    fn saved(width: u32, height: u32, alpha: bool, format: ImageFormat) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let picture = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                width,
                height,
                image::Rgba([200, 100, 50, 128]),
            ))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                width,
                height,
                image::Rgb([200, 100, 50]),
            ))
        };
        picture
            .save_with_format(dir.path().join("picture"), format)
            .unwrap();
        dir
    }

    #[test]
    fn a_picture_is_cropped_and_sized_as_discords_client_sends_it() {
        let dir = saved(3000, 1000, false, ImageFormat::Jpeg);
        let avatar =
            read_picture(&dir.path().join("picture"), ProfilePicture::Avatar, None).unwrap();
        assert_eq!(avatar.mime, "image/jpeg");
        let sent = image::load_from_memory(&avatar.data).unwrap();
        assert_eq!(
            sent.dimensions(),
            (1000, 1000),
            "the middle square, not enlarged"
        );
        assert_eq!(
            (avatar.preview.width, avatar.preview.height),
            AVATAR_PREVIEW
        );

        let banner =
            read_picture(&dir.path().join("picture"), ProfilePicture::Banner, None).unwrap();
        let sent = image::load_from_memory(&banner.data).unwrap();
        assert_eq!(
            sent.dimensions(),
            BANNER,
            "17 by 6, shrunk to Discord's largest"
        );
        assert_eq!(
            (banner.preview.width, banner.preview.height),
            BANNER_PREVIEW
        );

        let dir = saved(64, 64, true, ImageFormat::Png);
        let avatar =
            read_picture(&dir.path().join("picture"), ProfilePicture::Avatar, None).unwrap();
        assert_eq!(avatar.mime, "image/png", "transparency stays");
    }

    #[test]
    fn a_framed_picture_is_cut_where_it_was_framed() {
        // Red on the left half, blue on the right.
        let dir = tempfile::tempdir().unwrap();
        let picture = image::RgbImage::from_fn(400, 200, |x, _| {
            if x < 200 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        picture
            .save_with_format(dir.path().join("picture"), ImageFormat::Png)
            .unwrap();
        let chosen = PictureCrop {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let avatar = read_picture(
            &dir.path().join("picture"),
            ProfilePicture::Avatar,
            Some(chosen),
        )
        .unwrap();
        let sent = image::load_from_memory(&avatar.data).unwrap().into_rgb8();
        assert_eq!(sent.dimensions(), (200, 200), "the right half, square");
        let middle = sent.get_pixel(100, 100);
        assert!(middle[2] > 200 && middle[0] < 50, "blue: {middle:?}");
    }

    #[test]
    fn a_frame_keeps_the_profiles_shape_within_the_picture() {
        let chosen = PictureCrop {
            x: 0.9,
            y: 0.9,
            width: 0.5,
            height: 0.5,
        };
        assert_eq!(chosen.within(1000, 500, AVATAR), (500, 0, 500, 500));
        assert_eq!(
            chosen.within(1000, 1000, BANNER),
            (500, 824, 500, 176),
            "17 by 6, as far down as it goes"
        );
    }

    #[test]
    fn a_framed_gif_is_cut_frame_by_frame() {
        use image::{
            AnimationDecoder,
            codecs::gif::{GifDecoder, GifEncoder, Repeat},
        };
        let dir = tempfile::tempdir().unwrap();
        let mut file = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut file);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for shade in [0u8, 255] {
                let frame = image::RgbaImage::from_pixel(300, 100, image::Rgba([shade, 0, 0, 255]));
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        std::fs::write(dir.path().join("moving.gif"), &file).unwrap();
        let chosen = PictureCrop {
            x: 0.25,
            y: 0.0,
            width: 1.0 / 3.0,
            height: 1.0,
        };
        let avatar = read_picture(
            &dir.path().join("moving.gif"),
            ProfilePicture::Avatar,
            Some(chosen),
        )
        .unwrap();
        assert_eq!(avatar.mime, "image/gif");
        let frames = GifDecoder::new(std::io::Cursor::new(&avatar.data[..]))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(frames.len(), 2, "still moving");
        assert_eq!(frames[0].buffer().dimensions(), (100, 100));
    }

    #[test]
    fn what_isnt_a_picture_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(
            read_picture(&path, ProfilePicture::Avatar, None).unwrap_err(),
            "notes.txt isn't a PNG, JPG, GIF or WebP picture."
        );
        assert!(read_picture(&dir.path().join("gone.png"), ProfilePicture::Avatar, None).is_err());
    }

    #[test]
    fn a_change_goes_where_discords_client_sends_it() {
        let picture = PickedPicture {
            mime: "image/png",
            data: Arc::from(&b"png"[..]),
            preview: Arc::default(),
        };
        let change = ProfileChange {
            name: Some(String::new()),
            pronouns: Some("they/them".into()),
            avatar: Some(Some(picture)),
            banner: Some(None),
            ..Default::default()
        };
        let (member, profile) = server_bodies(&change);
        assert_eq!(
            member,
            serde_json::json!({"nick": "", "avatar": "data:image/png;base64,cG5n"})
        );
        assert_eq!(
            profile,
            serde_json::json!({"banner": null, "pronouns": "they/them"})
        );

        let change = ProfileChange {
            accent: Some(Some(0x5865f2)),
            bio: Some("Hi".into()),
            ..change
        };
        let (user, profile) = main_bodies(&change);
        assert_eq!(
            user,
            serde_json::json!({"global_name": "", "avatar": "data:image/png;base64,cG5n"})
        );
        assert_eq!(
            profile,
            serde_json::json!({"bio": "Hi", "pronouns": "they/them", "banner": null, "accent_color": 5_793_266})
        );
        let (_, profile) = main_bodies(&ProfileChange {
            accent: Some(None),
            ..Default::default()
        });
        assert_eq!(profile, serde_json::json!({"accent_color": null}));
    }

    /// Discord's editor sends the tag to its own endpoint: `primary_guild_id`
    /// in `PATCH /users/@me` isn't what its Save does.
    #[test]
    fn a_server_tag_stays_out_of_the_user_and_profile_bodies() {
        for primary_server in [Some("9".to_owned()), None] {
            let (user, profile) = main_bodies(&ProfileChange {
                primary_server: Some(primary_server),
                ..Default::default()
            });
            assert_eq!(user, serde_json::json!({}));
            assert_eq!(profile, serde_json::json!({}));
        }
    }

    #[test]
    fn a_theme_and_name_style_go_where_discords_editor_sends_them() {
        let (user, profile) = main_bodies(&ProfileChange {
            theme: Some(Some([0x112233, 0x445566])),
            name_style: Some(Some(crate::chat::NameStyle::new(
                4,
                2,
                &[0xff0000, 0x00ff00],
            ))),
            ..Default::default()
        });
        assert_eq!(
            user,
            serde_json::json!({"display_name_font_id": 4, "display_name_effect_id": 2, "display_name_colors": [0xff0000, 0x00ff00]})
        );
        assert_eq!(
            profile,
            serde_json::json!({"theme_colors": [0x112233, 0x445566]})
        );
        let (user, _) = main_bodies(&ProfileChange {
            name_style: Some(None),
            ..Default::default()
        });
        assert_eq!(
            user,
            serde_json::json!({"display_name_font_id": null, "display_name_effect_id": null, "display_name_colors": null})
        );
    }
}
