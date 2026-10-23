# Test issuer fixtures

For the drivers' OpenID Connect tests (`scripts/driver-tests.sh`): the key
set of a test issuer, `https://issuer.nucleus.test`, and one token it signed
(EdDSA) for the audience `nucleus-driver-tests`, the subject
`drivers_oidc`, valid until 2100.  The private key was thrown away once the
token was made, so nothing else can be signed with it.  A server accepts
this token only when started with exactly this issuer, audience and key set
— as the driver tests start one.
