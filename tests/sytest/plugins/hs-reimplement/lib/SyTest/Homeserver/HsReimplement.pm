# Sytest homeserver plugin for this project's server (PLAN.md section 12 layer L4;
# docs/workstreams/14-test-and-conformance.md). Untested: no `hs-server` binary exists in this
# workspace yet, so `start` below cannot succeed. Structure and the `_start_process_and_await_*`
# usage are adapted from refs/sytest/lib/SyTest/Homeserver/Dendrite.pm (Apache-2.0, Sytest project,
# Copyright New Vector Ltd) -- Dendrite is the closest existing analog (a single, from-scratch
# monolith binary, not a Python framework like Synapse.pm's target), read for the shape of "start
# one binary, wait for it to become connectable" rather than copied verbatim; every value below is
# specific to this project.
#
# See tests/sytest/README.md for how Sytest discovers this file (the SYTEST_PLUGINS mechanism),
# and for what remains to fill in once a server binary exists (the TODOs below).

use strict;
use warnings;

use Future;

package SyTest::Homeserver::HsReimplement;
use base qw( SyTest::Homeserver );

use Carp;
use POSIX qw( WIFEXITED WEXITSTATUS );

sub _init
{
   my $self = shift;
   my ( $args ) = @_;

   $self->{$_} = delete $args->{$_} for qw( bindir );
   defined $self->{bindir} or croak "Need a bindir (path to the hs-server binary's directory)";

   my $idx = $self->{hs_index};
   $self->{ports} = {
      federation => main::alloc_port( "hs-reimplement[$idx].federation" ),
      client     => main::alloc_port( "hs-reimplement[$idx].client" ),
   };
   $self->{paths} = {};

   $self->SUPER::_init( $args );
}

sub secure_port   { return $_[0]->{ports}{federation}; }
sub unsecure_port { return $_[0]->{ports}{client}; }
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

# Generates a throwaway self-signed TLS cert/key for the federation listener with `openssl`,
# matching the recipe tests/complement/startup.sh uses for Complement's CA-signed variant --
# Sytest has no shared CA to sign against, so this is a bare self-signed cert instead, which is
# what Sytest's own homeservers typically use for each other (Sytest disables federation TLS
# validation between its own homeserver instances).
sub _generate_tls_keyfiles
{
   my $self = shift;
   my $hs_dir = $self->{hs_dir};
   $self->{paths}{tls_cert} = "$hs_dir/server.crt";
   $self->{paths}{tls_key}  = "$hs_dir/server.key";

   return Future->done if -f $self->{paths}{tls_cert} && -f $self->{paths}{tls_key};

   $self->{output}->diag( "Generating a self-signed TLS cert for " . $self->server_name );
   return $self->_run_command(
      command => [
         'openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
         '-keyout', $self->{paths}{tls_key},
         '-out', $self->{paths}{tls_cert},
         '-days', '1',
         '-subj', "/CN=" . $self->{bind_host},
      ],
   );
}

sub start
{
   my $self = shift;

   my $hs_dir = $self->{hs_dir};
   # TODO: replace with the real config format once hs-config (track 13) defines one Sytest
   # should write; a plain YAML/TOML file at this path, matching whatever `hs-server --config`
   # expects, is the expected shape based on every other implementation's plugin.
   $self->{paths}{config} = "$hs_dir/hs-reimplement.yaml";
   $self->{paths}{data_dir} = "$hs_dir/data";
   mkdir $self->{paths}{data_dir} unless -d $self->{paths}{data_dir};

   return $self->_generate_tls_keyfiles->then( sub {
      $self->_start_server;
   });
}

# TODO: this command line is a placeholder matching tests/complement/startup.sh's expected
# `hs-server` invocation shape; update both together once the real CLI exists (docs/status/
# 14-test-and-conformance.md tracks who owns assembling that binary).
sub _start_server
{
   my $self = shift;
   my $output = $self->{output};
   my $idx = $self->{hs_index};

   my @command = (
      $self->{bindir} . '/hs-server',
      '--server-name', $self->server_name,
      '--client-listen', $self->{bind_host} . ':' . $self->unsecure_port,
      '--federation-listen', $self->{bind_host} . ':' . $self->secure_port,
      '--federation-tls-cert', $self->{paths}{tls_cert},
      '--federation-tls-key', $self->{paths}{tls_key},
      '--data-dir', $self->{paths}{data_dir},
      '--registration-shared-secret', 'sytest',
      '--config', $self->{paths}{config},
   );

   $output->diag( "Starting hs-reimplement with: @command" );

   return $self->_start_process_and_await_connectable(
      command => [ @command ],
      connect_host => $self->{bind_host},
      connect_port => $self->unsecure_port,
      name => "hs-reimplement-$idx",
   )->else( sub {
      die "Unable to start hs-reimplement: $_[0]\n";
   })->on_done( sub {
      $output->diag( "Started hs-reimplement" );
   });
}

1;
