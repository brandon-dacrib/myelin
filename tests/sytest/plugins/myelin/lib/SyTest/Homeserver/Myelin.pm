# Sytest homeserver plugin for this project's `hs` binary (PLAN.md section 12 layer L4;
# docs/workstreams/14-test-and-conformance.md). Shape adapted from Sytest's
# lib/SyTest/Homeserver/Dendrite.pm and the haproxy front of lib/SyTest/Homeserver/Synapse.pm
# (both Apache-2.0, Copyright New Vector Ltd): start one binary, wait until it accepts
# connections. The configuration and every value below are this project's own.
#
# Each Sytest homeserver is two processes:
#
#   hs serve      plaintext, on the "unsecure" port, serving client, federation, media and
#                 health resources on one router (127.0.0.1 only);
#   haproxy       TLS on the "secure" port, forwarding to the plaintext port.
#
# Sytest talks to every homeserver over HTTPS on its secure port (public_baseurl) and the
# servers federate with each other there, so the server name is `localhost:<secure port>`.
# `hs serve` does not terminate TLS itself (crates/hs-cli/src/serve.rs warns and binds
# plaintext), which is why tests/complement/ puts stunnel in front and this puts haproxy, which
# Sytest's image already has, in front.
#
# Certificates are verified, unlike Sytest's Synapse and Dendrite configurations (which turn
# verification off). Each server's certificate is signed by Sytest's own test CA
# (keys/ca.crt in the Sytest checkout, the CA that also signs Sytest's federation and HTTPS test
# servers), outbound federation trusts that CA through `federation.custom_ca_certificates`, and
# myelin_sytest.sh adds it to the container's trust store for the clients that use the system
# roots (appservice transactions to Sytest's test server). With verification off for federation
# only, every appservice test failed with "tlsv1 alert unknown ca" (2026-10-01).
#
# The server's output goes to `<hs_dir>/hs.log` (and haproxy's to `haproxy.log`), which
# myelin_sytest.sh copies to /logs/server-N/ after the run.

use strict;
use warnings;

use Future;

package SyTest::Homeserver::Myelin;
use base qw( SyTest::Homeserver );

use Carp;
use Cwd ();
use JSON ();
use File::Slurper qw( read_binary );
use SyTest::SSL ();

sub _init
{
   my $self = shift;
   my ( $args ) = @_;

   $self->{binary} = delete $args->{binary};
   defined $self->{binary} or croak "Need the path of the hs binary";

   my $idx = $args->{hs_index};
   $self->{ports} = {
      secure   => main::alloc_port( "myelin[$idx].secure" ),
      unsecure => main::alloc_port( "myelin[$idx].unsecure" ),
   };
   $self->{paths} = {};

   $self->SUPER::_init( $args );
}

sub secure_port     { return $_[0]->{ports}{secure}; }
sub unsecure_port   { return $_[0]->{ports}{unsecure}; }
sub federation_port { return $_[0]->secure_port; }
sub federation_host { return $_[0]->{bind_host}; }

sub server_name
{
   my $self = shift;
   return $self->{bind_host} . ":" . $self->secure_port;
}

sub public_baseurl
{
   my $self = shift;
   return "https://" . $self->{bind_host} . ":" . $self->secure_port;
}

sub start
{
   my $self = shift;
   my $hs_dir = $self->{hs_dir};

   $self->{paths}{tls_cert}    = "$hs_dir/server.crt";
   $self->{paths}{tls_key}     = "$hs_dir/server.key";
   $self->{paths}{signing_dir} = "$hs_dir/signing-keys";
   $self->{paths}{data_dir}    = "$hs_dir/data";
   $self->{paths}{log}         = "$hs_dir/hs.log";
   -d $_ or mkdir $_ for $self->{paths}{signing_dir}, $self->{paths}{data_dir};

   $self->{paths}{config} = $self->write_yaml_file( "myelin.yaml" => $self->_get_config );

   return $self->_generate_tls_keyfiles
      ->then( sub { $self->_generate_signing_key } )
      ->then( sub { $self->_start_hs } )
      ->then( sub { $self->_start_haproxy } );
}

# The native configuration (crates/hs-config) for one Sytest homeserver. The settings that
# differ from the defaults are the ones Sytest's Synapse configuration (Synapse.pm) also changes:
# open registration, guest access, Sytest's identity server, the shared secret `reg_secret`, no rate limits, no IP-range blocklists (every
# server is on localhost), public rooms over federation, and the appservice registrations Sytest
# writes for server 0.
sub _get_config
{
   my $self = shift;

   return {
      server => {
         server_name      => $self->server_name,
         public_baseurl   => $self->public_baseurl . "/",
         signing_key_path => $self->{paths}{signing_dir},
      },
      storage => {
         backend  => "embedded",
         data_dir => $self->{paths}{data_dir},
      },
      listeners => {
         listeners => [ {
            port           => $self->unsecure_port,
            bind_addresses => [ "127.0.0.1" ],
            resources      => [ "client", "federation", "media", "health" ],
         } ],
      },
      auth => {
         enable_registration        => JSON::true,
         allow_guest_access         => JSON::true,
         # Sytest's identity server listens on localhost, on a port chosen per run.
         identity_servers           => [ "localhost", "127.0.0.1" ],
         enable_legacy_login        => JSON::true,
         registration_shared_secret => "reg_secret",
      },
      network => {
         # Sytest's own federation server and identity server listen on `localhost`, which
         # the image's Perl binds on `::1`; the server's default (IPv4 only, decision of
         # 2026-10-02) could not reach them, so its key fetches failed and every signed request
         # from Sytest's server was answered `401` (inbound federation 0 of ~60).
         outbound => { ipv4_only => JSON::false },
      },
      federation => {
         custom_ca_certificates             => [ _sytest_ca() ],
         ip_range_blocklist                 => [],
         allow_public_rooms_over_federation => JSON::true,
      },
      media => {
         # The default is ./media-store, relative to the working directory, which is the
         # read-only Sytest checkout.
         storage => { backend => "local", path => $self->{hs_dir} . "/media-store" },
         url_preview_enabled           => JSON::true,
         url_preview_ip_range_blocklist => [],
      },
      rate_limits => {
         enabled => JSON::false,
      },
      appservices => {
         registration_files => $self->{app_service_config_files} // [],
      },
   };
}

# The certificate for the secure port, signed by Sytest's test CA with Sytest's own helper (the
# one its federation test servers use), so other servers can verify it.
sub _generate_tls_keyfiles
{
   my $self = shift;

   return Future->done if -f $self->{paths}{tls_cert} && -f $self->{paths}{tls_key};

   SyTest::SSL::ensure_ssl_key( $self->{paths}{tls_key} );
   SyTest::SSL::create_ssl_cert( $self->{paths}{tls_cert}, $self->{paths}{tls_key}, $self->{bind_host} );
   return Future->done;
}

# Sytest's test CA, which run-tests.pl (working directory: the Sytest checkout) reads as
# keys/ca.crt.
sub _sytest_ca
{
   return Cwd::abs_path( "keys/ca.crt" );
}

sub _generate_signing_key
{
   my $self = shift;
   my $key = $self->{paths}{signing_dir} . "/hs.signing.key";

   return Future->done if -f $key;

   return $self->_run_command(
      command => [ $self->{binary}, 'generate-signing-key', '-o', $key ],
   );
}

sub _start_hs
{
   my $self = shift;
   my $output = $self->{output};
   my $idx = $self->{hs_index};

   # Through a shell only to send the output to a file; `exec` keeps `hs` the process Sytest
   # signals when it stops the server.
   my $cmd = sprintf "exec '%s' serve -c '%s' >>'%s' 2>&1",
      $self->{binary}, $self->{paths}{config}, $self->{paths}{log};

   $output->diag( "Starting myelin hs-$idx: $cmd" );

   return $self->_start_process_and_await_connectable(
      # Sytest's fake identity server has a self-signed certificate with no subjectAltName,
      # which no verifying client accepts; this test-only switch is how Synapse's
      # `use_insecure_ssl_client_just_for_testing_do_not_use` is spelt here.
      setup        => [ env => { %ENV, RUST_LOG => $ENV{MYELIN_RUST_LOG} // "info",
                                 HS_TEST_INSECURE_IDENTITY_SERVER_TLS => "1" } ],
      command      => [ "/bin/sh", "-c", $cmd ],
      connect_host => "127.0.0.1",
      connect_port => $self->unsecure_port,
      name         => "myelin-$idx",
   )->else( sub {
      die "Unable to start myelin hs-$idx: $_[0]\n";
   })->on_done( sub {
      $output->diag( "Started myelin hs-$idx" );
   });
}

sub _start_haproxy
{
   my $self = shift;
   my $output = $self->{output};
   my $idx = $self->{hs_index};

   my $cert = read_binary( $self->{paths}{tls_cert} );
   my $key  = read_binary( $self->{paths}{tls_key} );
   $self->{paths}{pem} = $self->write_file( "combined.pem", $cert . $key );

   my $bind_host = $self->{bind_host};
   my $secure    = $self->secure_port;
   my $unsecure  = $self->unsecure_port;
   my $pem       = $self->{paths}{pem};

   # Listen on both loopback families when Sytest's bind host is `localhost`. haproxy binds the
   # first address the name resolves to, which on the Sytest image is `::1`, and since
   # 2026-10-02 the server's outbound connections are IPv4 only by default
   # (`network.outbound.ipv4_only`), so a proxy on `::1` alone refused every federation request
   # the other server made (444/772 on 2026-10-04, "Connection refused" on `/invite`), while
   # Sytest's own client reached it over IPv6 and saw nothing wrong.
   my $binds = $bind_host eq "localhost"
      ? "    bind 127.0.0.1:${secure} ssl crt ${pem}\n    bind [::1]:${secure} ssl crt ${pem}"
      : "    bind ${bind_host}:${secure} ssl crt ${pem}";

   # Timeouts longer than Sytest's longest long-poll (/sync and /events wait up to 60 s, and
   # some tests ask for more); `option http-server-close` so a client's keep-alive does not pin
   # a backend connection the server already closed.
   my $config = $self->write_file( "haproxy.conf", <<"EOCONFIG" );
global
    maxconn 2000
    # One thread. With one per core, on a loaded desktop haproxy's watchdog killed it mid-run
    # (a thread "stuck" 2.7 s of CPU in SSL_free under lock contention), and every test after
    # that failed with "connection refused".
    nbthread 1

defaults
    mode http
    timeout connect 5s
    timeout client 180s
    timeout server 180s
    option http-server-close
    option forwardfor

frontend https-in
${binds}
    http-request set-header X-Forwarded-Proto https
    default_backend myelin

backend myelin
    server myelin 127.0.0.1:${unsecure}
EOCONFIG

   my $cmd = sprintf "exec /usr/sbin/haproxy -db -f '%s' >>'%s' 2>&1",
      $config, $self->{hs_dir} . "/haproxy.log";

   return $self->_start_process_and_await_connectable(
      command      => [ "/bin/sh", "-c", $cmd ],
      connect_host => $bind_host,
      connect_port => $secure,
      name         => "haproxy-$idx",
   )->else( sub {
      die "Unable to start haproxy for myelin hs-$idx: $_[0]\n";
   })->on_done( sub {
      $output->diag( "Started haproxy for myelin hs-$idx on $secure" );
   });
}

1;
