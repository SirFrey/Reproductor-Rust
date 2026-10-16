//! One-time cleanup of the old Google OAuth sign-in, which the YouTube Music
//! session (see `ytlogin`) has replaced: revoke its refresh token at Google and
//! remove it from Windows Credential Manager.

const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const KEYRING_SERVICE: &str = "reproductor-rs";
const KEYRING_USER: &str = "google-refresh-token";

pub fn remove_legacy_login() {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) else { return };
    let Ok(token) = entry.get_password() else { return };
    // Delete locally even if the revoke request fails (e.g. offline).
    let _ = ureq::post(REVOKE_URL).send_form([("token", token.as_str())]);
    let _ = entry.delete_credential();
}
