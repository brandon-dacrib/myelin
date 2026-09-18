# Factory half of the Sytest plugin: registers this implementation's name ("HsReimplement", so
# `run-tests.pl -I HsReimplement` selects it) and its command-line options. See
# SyTest::Homeserver::HsReimplement in ../Homeserver/HsReimplement.pm for what actually starts the
# server, and tests/sytest/README.md for how Sytest discovers this file at all. Adapted in shape
# from refs/sytest/lib/SyTest/HomeserverFactory/Dendrite.pm (Apache-2.0, Sytest project, Copyright
# New Vector Ltd); values are this project's own.

use strict;
use warnings;

require SyTest::Homeserver::HsReimplement;

package SyTest::HomeserverFactory::HsReimplement;
use base qw( SyTest::HomeserverFactory );

sub _init
{
   my $self = shift;

   $self->{args} = {
      bindir => "../target/release",
   };

   $self->SUPER::_init( @_ );
}

sub implementation_name
{
   return "hs-reimplement";
}

sub get_options
{
   my $self = shift;

   return (
      'hs-reimplement-binary-directory=s' => \$self->{args}{bindir},
      $self->SUPER::get_options(),
   );
}

sub print_usage
{
   print STDERR <<EOF
   --hs-reimplement-binary-directory DIR  - path to the directory containing the
                                             hs-server binary (default: ../target/release,
                                             i.e. this workspace's target/release relative to a
                                             sytest checkout cloned as a sibling directory)
EOF
}

sub create_server
{
   my $self = shift;
   my %params = ( @_, %{ $self->{args} } );

   return SyTest::Homeserver::HsReimplement->new( %params );
}

1;
